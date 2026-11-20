//! 主 Agent 工具集：常驻工具（语音询问 / 交付 / 搜索）+ 渐进加载清单 ToolGuide。
//!
//! 采用 Pi 式渐进加载：常驻工具始终可见；ls/grep/find/curl/ImportSkill 及所有
//! 已安装 Skills 只在 ToolGuide 的说明里给出「名称 + 一句话」，模型需要时调用
//! ToolGuide 拿到完整用法并把它加入本会话可用集，下一次模型请求即可直接调用。

use crate::events::Event;
use crate::mcp::McpServerConfig;
use crate::state::CoreState;
use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Schema, Tool};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn make_tool(name: &str, description: &str, parameters: Value) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters: Schema::new(parameters),
        constrained_sampling: None,
    }
}

fn str_of(input: &Value, key: &str, default: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

// ============================================================
// auto_announce —— core 自动播报最终回复（不走 AI 工具调用）
// 由 run_main_turn 在每轮结束后直接调用：用本轮最终文本合成并播放。
// TTS 后台异步执行，不阻塞 agent 循环；speaking 原子标志与
// speak_start/speak_end 事件在后台任务内管理，前端展示语义不变。
// ============================================================

/// 从最终回复中提取「语音播报」段（提示词约定：回复末尾以 【Voice】 开头的一段，
/// 专供 TTS 朗读，口语化 ≤100 字）。无标记时回退播报全文（兼容旧回复/未遵守格式的情况）。
/// 注意：不在此处强行截断——长内容必须完整念完，避免播报中途戛然而止。
fn extract_announce_text(raw: &str) -> String {
    // 提示词约定标记：新版用英文【Voice】（星舰风格），同时兼容旧版【语音播报】。
    const MARKER: &str = "【Voice】";
    const LEGACY: &str = "【语音播报】";
    // 找到标记 → 只取标记后的朗读内容；标记后为空（空标记）也回退全文，
    // 保证用户最终总能听到内容；无标记 → 回退 AI 最终回复全文。
    let text = if let Some(pos) = raw.find(MARKER) {
        let t = raw[pos + MARKER.len()..].trim().to_string();
        if t.is_empty() {
            raw.trim().to_string()
        } else {
            t
        }
    } else if let Some(pos) = raw.find(LEGACY) {
        let t = raw[pos + LEGACY.len()..].trim().to_string();
        if t.is_empty() {
            raw.trim().to_string()
        } else {
            t
        }
    } else {
        raw.trim().to_string()
    };
    strip_markdown_for_tts(&text).trim().to_string()
}

fn strip_markdown_for_tts(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut l = line.trim().to_string();
        let trimmed = l.trim_start();
        let mut chars = trimmed.chars();
        let first = chars.next();
        let second = chars.next();
        // 行首标记：# 标题、-/* 列表、> 引用、| 表格、` 代码围栏
        let is_line_mark = matches!(
            (first, second),
            (Some('#'), _)
                | (Some('-'), _)
                | (Some('*'), _)
                | (Some('＊'), _)
                | (Some('>'), _)
                | (Some('|'), _)
                | (Some('`'), _)
        );
        // 数字列表：1. / 1、 / 1) 开头
        let mut is_num_mark = false;
        if let Some(c) = first {
            if c.is_ascii_digit() {
                let mut it = trimmed.chars().skip(1).peekable();
                let mut digits = 1;
                while let Some(&d) = it.peek() {
                    if d.is_ascii_digit() {
                        digits += 1;
                        it.next();
                    } else {
                        break;
                    }
                }
                if let Some(&d) = it.peek() {
                    if matches!(d, '.' | '、' | ')') && digits > 0 {
                        is_num_mark = true;
                    }
                }
            }
        }
        if is_line_mark || is_num_mark {
            l = trimmed
                .chars()
                .skip_while(|c| {
                    matches!(c, '#' | '-' | '*' | '>' | '|' | '`')
                        || c.is_ascii_digit()
                        || matches!(c, '.' | '、' | ')' | ' ' | '\t')
                })
                .collect::<String>();
        }
        // 行内：去掉强调/代码标记，链接 `[文字](url)` 还原为文字
        let mut cleaned = String::with_capacity(l.len());
        let mut it = l.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                // ASCII 强调符 + 全角星号/序号符等 TTS 会念成"星号/杠"的符号，一律跳过
                '*' | '_' | '~' | '`' | '＊' | '※' | '·' | '•' | '◇' | '◆' | '→' => { /* 跳过 */ }
                '[' => {
                    let mut buf = String::new();
                    let mut found = false;
                    while let Some(&n) = it.peek() {
                        if n == ']' {
                            it.next();
                            if it.peek() == Some(&'(') {
                                it.next();
                                while let Some(&m) = it.peek() {
                                    if m == ')' {
                                        it.next();
                                        found = true;
                                        break;
                                    }
                                    it.next();
                                }
                            }
                            break;
                        }
                        buf.push(n);
                        it.next();
                    }
                    if found {
                        cleaned.push_str(&buf);
                    } else {
                        cleaned.push('[');
                        cleaned.push_str(&buf);
                    }
                }
                _ => cleaned.push(c),
            }
        }
        lines.push(cleaned);
    }
    lines.join("\n")
}

pub async fn auto_announce(core: Arc<CoreState>, text: String) {
    let text = extract_announce_text(&text);
    tracing::info!(
        "auto_announce text ({} chars): {}",
        text.chars().count(),
        text
    );
    if text.is_empty() {
        return;
    }
    // 互斥：已有语音正在播报时跳过本次（不排队、不累积）
    if core.speaking.load(Ordering::SeqCst) {
        tracing::info!("自动播报跳过：已有语音正在播报");
        return;
    }
    core.emit(Event::Voice {
        kind: "speak_start".into(),
        text: text.clone(),
    });
    core.speaking.store(true, Ordering::SeqCst);

    // TTS 后台异步播报，不阻塞主 Agent 循环（main_busy 只覆盖 LLM+工具逻辑）
    tokio::spawn(async move {
        let (voice, rate) = {
            let c = core.config.lock().await;
            (c.effective_voice(), c.rate)
        };
        let text2 = text.clone();
        // 打断优先：播报前再确认一次，避免打断后 AI 仍继续说话
        let result: Result<(), String> = if core.interrupt_main.load(Ordering::SeqCst) {
            tracing::info!("打断生效：跳过 edge-tts 播报");
            Ok(())
        } else {
            core.voice
                .speak(&text2, &voice, rate)
                .await
                .map_err(|e| e.to_string())
        };
        core.speaking.store(false, Ordering::SeqCst);
        core.emit(Event::Voice {
            kind: "speak_end".into(),
            text: text2,
        });
        if let Err(e) = result {
            // 播报失败：不重试（避免死循环），记录日志并推送前端提示，
            // 否则用户只看到"播报中"闪一下就没了，不知道是没有声音还是坏了。
            tracing::warn!("TTS 自动播报失败: {e}");
            core.emit(Event::Voice {
                kind: "speak_error".into(),
                text: format!("语音播报失败：{e}"),
            });
        }
    });
}

// ============================================================
// ToolHub —— 渐进加载：懒加载工具清单 + 各会话已启用集合
// ============================================================

pub struct ToolHub {
    core: Arc<CoreState>,
    lazy: Vec<Arc<dyn AgentTool>>,
    /// 会话 id → 已启用（已向模型暴露 schema）的懒加载工具名
    loaded: Mutex<HashMap<String, HashSet<String>>>,
}

impl ToolHub {
    pub fn new(core: Arc<CoreState>) -> Arc<Self> {
        let lazy: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(LsTool),
            Arc::new(GrepTool),
            Arc::new(FindTool),
            Arc::new(CurlTool),
            Arc::new(ImportSkill::new(core.clone())),
            Arc::new(ConnectMcp::new(core.clone())),
        ];
        Arc::new(Self {
            core,
            lazy,
            loaded: Mutex::new(HashMap::new()),
        })
    }

    pub fn core(&self) -> &Arc<CoreState> {
        &self.core
    }

    /// 清单里可请求的懒加载工具定义
    pub fn lazy_schemas(&self) -> Vec<Tool> {
        self.lazy.iter().map(|t| t.schema().clone()).collect()
    }

    /// 全部懒加载工具实例（eager 模式：默认全部可用）
    pub fn all_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.lazy.clone()
    }

    /// 本会话已启用的懒加载工具实例
    pub async fn loaded_tools(&self, sid: &str) -> Vec<Arc<dyn AgentTool>> {
        let loaded = self.loaded.lock().await;
        let Some(names) = loaded.get(sid) else {
            return Vec::new();
        };
        self.lazy
            .iter()
            .filter(|t| names.contains(&t.schema().name))
            .cloned()
            .collect()
    }

    /// 本会话已启用的懒加载工具 schema
    pub async fn loaded_schemas(&self, sid: &str) -> Vec<Tool> {
        self.loaded_tools(sid)
            .await
            .iter()
            .map(|t| t.schema().clone())
            .collect()
    }

    /// 把某个懒加载工具加入本会话可用集；返回是否命中
    pub async fn enable(&self, sid: &str, name: &str) -> bool {
        if !self.lazy.iter().any(|t| t.schema().name == name) {
            return false;
        }
        let mut loaded = self.loaded.lock().await;
        loaded
            .entry(sid.to_string())
            .or_default()
            .insert(name.to_string());
        true
    }

    /// 清空全部会话的加载状态（设置/技能变更后调用）
    pub async fn reset(&self) {
        self.loaded.lock().await.clear();
    }
}

// ============================================================
// ToolGuide —— 唯一的渐进加载入口
// ============================================================

pub struct ToolGuide {
    hub: Arc<ToolHub>,
    sid: String,
    schema: Tool,
}

impl ToolGuide {
    pub async fn new(hub: Arc<ToolHub>, sid: String) -> Arc<Self> {
        let mut index = String::new();
        for t in hub.lazy_schemas() {
            index.push_str(&format!("- {}：{}\n", t.name, first_line(&t.description)));
        }
        for s in hub.core().skills.list().await.iter().filter(|s| s.enabled) {
            index.push_str(&format!("- skill:{}：{}\n", s.name, s.description));
        }
        let description = format!(
            "渐进式工具清单。需要清单中某项能力时，先调用本工具请求其名称，\
拿到完整用法后该工具会加入本会话、可在下一次请求直接调用。\n清单：\n{index}"
        );
        let schema = make_tool(
            "ToolGuide",
            &description,
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "要启用的工具名或技能名（技能写作 skill:名称）"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        );
        Arc::new(Self { hub, sid, schema })
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(60)
        .collect()
}

#[async_trait]
impl AgentTool for ToolGuide {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "ToolGuide"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let name = str_of(&params, "name", "");
        let name = name.trim();
        if name.is_empty() {
            return Err(AgentError::tool("缺少工具名 name"));
        }
        // 1) 懒加载工具：启用并返回完整说明
        if let Some(tool) = self
            .hub
            .lazy_schemas()
            .into_iter()
            .find(|t| t.name == name)
        {
            self.hub.enable(&self.sid, name).await;
            return Ok(AgentToolResult::text(format!(
                "已启用工具 {}，下一次请求即可调用。\n说明：{}\n参数结构：{}",
                tool.name,
                tool.description,
                serde_json::to_string(&tool.parameters.as_value()).unwrap_or_default()
            )));
        }
        // 2) 技能：直接返回完整说明文档
        let skill = self
            .hub
            .core()
            .skills
            .list()
            .await
            .into_iter()
            .find(|s| s.enabled && s.name == name);
        if let Some(skill) = skill {
            let usage = crate::skills::read_usage(&skill.path);
            return Ok(AgentToolResult::text(if usage.trim().is_empty() {
                format!("技能「{}」：{}（路径：{}）", skill.name, skill.description, skill.path.display())
            } else {
                format!("技能「{}」完整说明：\n{usage}", skill.name)
            }));
        }
        Err(AgentError::tool(format!(
            "清单中没有「{name}」。可用的工具：{}；可用技能：{}",
            self.hub
                .lazy_schemas()
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.hub
                .core()
                .skills
                .list()
                .await
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

// ============================================================
// 懒加载工具实现（ls / grep / find / curl / ImportSkill）
// ============================================================

/// ls：列出目录内容。复用系统 read_dir，路径支持绝对/相对 home。
struct LsTool;

#[async_trait]
impl AgentTool for LsTool {
    fn schema(&self) -> &Tool {
        static SCHEMA: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            make_tool(
                "ls",
                "列出目录下的文件与子目录（名称、类型、大小、修改时间）。",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "目录路径（默认当前用户主目录）"}
                    },
                    "additionalProperties": false
                }),
            )
        })
    }
    fn label(&self) -> &str {
        "ls"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let raw = str_of(&params, "path", "");
        let path = if raw.trim().is_empty() {
            crate::paths::home_dir()
        } else {
            std::path::PathBuf::from(raw.trim())
        };
        let entries = std::fs::read_dir(&path)
            .map_err(|e| AgentError::tool(format!("无法读取目录 {}：{e}", path.display())))?;
        let mut rows: Vec<String> = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let ft = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            rows.push(if ft {
                format!("{name}/")
            } else {
                format!("{name}  ({size} B)")
            });
        }
        rows.sort();
        if rows.is_empty() {
            return Ok(AgentToolResult::text(format!("目录 {} 为空", path.display())));
        }
        Ok(AgentToolResult::text(format!(
            "{}（{} 项）：\n{}",
            path.display(),
            rows.len(),
            rows.join("\n")
        )))
    }
}

/// grep：在目录内递归搜索文本（简单字面量匹配，忽略二进制/隐藏目录）。
struct GrepTool;

#[async_trait]
impl AgentTool for GrepTool {
    fn schema(&self) -> &Tool {
        static SCHEMA: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            make_tool(
                "grep",
                "在目录中递归搜索文本，返回命中的文件、行号与内容。",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "要搜索的文本"},
                        "path": {"type": "string", "description": "搜索根目录（默认用户主目录）"},
                        "max_results": {"type": "number", "description": "最多返回条数（默认 40）"}
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            )
        })
    }
    fn label(&self) -> &str {
        "grep"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let pattern = str_of(&params, "pattern", "");
        if pattern.trim().is_empty() {
            return Err(AgentError::tool("缺少搜索内容 pattern"));
        }
        let raw = str_of(&params, "path", "");
        let root = if raw.trim().is_empty() {
            crate::paths::home_dir()
        } else {
            std::path::PathBuf::from(raw.trim())
        };
        let max = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(40)
            .min(200) as usize;
        let mut hits: Vec<String> = Vec::new();
        grep_dir(&root, pattern.trim(), max, 0, &mut hits);
        if hits.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "在 {} 下未找到「{pattern}」",
                root.display()
            )));
        }
        Ok(AgentToolResult::text(hits.join("\n")))
    }
}

fn grep_dir(dir: &std::path::Path, needle: &str, max: usize, depth: usize, out: &mut Vec<String>) {
    if out.len() >= max || depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= max {
            return;
        }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            grep_dir(&p, needle, max, depth + 1, out);
        } else if ft.is_file() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > 2 * 1024 * 1024 {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if line.contains(needle) {
                    out.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                    if out.len() >= max {
                        return;
                    }
                }
            }
        }
    }
}

/// find：按名称通配符查找文件。
struct FindTool;

#[async_trait]
impl AgentTool for FindTool {
    fn schema(&self) -> &Tool {
        static SCHEMA: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            make_tool(
                "find",
                "按文件名通配符（如 *.rs、report*）查找文件。",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "文件名通配符，支持 * 与 ?"},
                        "path": {"type": "string", "description": "搜索根目录（默认用户主目录）"},
                        "max_results": {"type": "number", "description": "最多返回条数（默认 40）"}
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            )
        })
    }
    fn label(&self) -> &str {
        "find"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let pattern = str_of(&params, "pattern", "");
        if pattern.trim().is_empty() {
            return Err(AgentError::tool("缺少文件名模式 pattern"));
        }
        let raw = str_of(&params, "path", "");
        let root = if raw.trim().is_empty() {
            crate::paths::home_dir()
        } else {
            std::path::PathBuf::from(raw.trim())
        };
        let max = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(40)
            .min(200) as usize;
        let mut hits = Vec::new();
        find_dir(&root, pattern.trim(), max, 0, &mut hits);
        if hits.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "在 {} 下未找到匹配 {pattern} 的文件",
                root.display()
            )));
        }
        Ok(AgentToolResult::text(hits.join("\n")))
    }
}

/// 简单通配符匹配：* 任意串，? 单字符。
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn find_dir(dir: &std::path::Path, pattern: &str, max: usize, depth: usize, out: &mut Vec<String>) {
    if out.len() >= max || depth > 10 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if out.len() >= max {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if wildcard_match(pattern, &name) {
            out.push(p.display().to_string());
        }
        if ft.is_dir() {
            find_dir(&p, pattern, max, depth + 1, out);
        }
    }
}

/// curl：抓取网页/接口原始内容。
struct CurlTool;

#[async_trait]
impl AgentTool for CurlTool {
    fn schema(&self) -> &Tool {
        static SCHEMA: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            make_tool(
                "curl",
                "抓取指定 URL 的原始内容（GET/POST），用于读取网页、接口或下载文本。",
                json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "完整 URL"},
                        "method": {"type": "string", "description": "GET 或 POST（默认 GET）"},
                        "body": {"type": "string", "description": "POST 请求体（可选）"}
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
            )
        })
    }
    fn label(&self) -> &str {
        "curl"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let url = str_of(&params, "url", "");
        if url.trim().is_empty() {
            return Err(AgentError::tool("缺少 URL"));
        }
        let method = str_of(&params, "method", "GET").to_uppercase();
        let body = params.get("body").and_then(|v| v.as_str()).map(|s| s.to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| AgentError::tool(e.to_string()))?;
        let mut req = if method == "POST" {
            client.post(url.trim())
        } else {
            client.get(url.trim())
        };
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = tokio::select! {
            r = req.send() => r.map_err(|e| AgentError::tool(format!("请求失败: {e}")))?,
            _ = signal.cancelled() => return Err(AgentError::Abort),
        };
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AgentError::tool(format!("读取响应失败: {e}")))?;
        let truncated: String = text.chars().take(8000).collect();
        Ok(AgentToolResult::text(format!(
            "HTTP {status}\n{truncated}{}",
            if text.chars().count() > 8000 { "\n…（已截断）" } else { "" }
        )))
    }
}

// ============================================================
// ImportSkill —— 让 AI 导入 skill 文件夹
// ============================================================

pub struct ImportSkill {
    core: Arc<CoreState>,
    schema: Tool,
}

impl ImportSkill {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: make_tool(
                "ImportSkill",
                "导入一个 skill 文件夹到 skills 系统。文件夹需包含运行文件与说明文档（如 SKILL.md）。输入文件夹绝对路径即可。",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "skill 文件夹的绝对路径"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

#[async_trait]
impl AgentTool for ImportSkill {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "ImportSkill"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let path = str_of(&params, "path", "");
        if path.is_empty() {
            return Err(AgentError::tool("缺少路径 path"));
        }
        match self.core.skills.import(&path).await {
            Ok(skill) => {
                self.core.emit(Event::SkillsUpdated);
                Ok(AgentToolResult::text(format!(
                    "已导入 skill「{}」：{}",
                    skill.name, skill.description
                )))
            }
            Err(e) => Err(AgentError::tool(format!("导入失败: {e}"))),
        }
    }
}

// ============================================================
// ConnectMcp —— AI 自主连接 MCP 服务（连接后写入连接器列表并立即探测）
// ============================================================

pub struct ConnectMcp {
    core: Arc<CoreState>,
    schema: Tool,
}

impl ConnectMcp {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: make_tool(
                "ConnectMcp",
                "连接一个 MCP（Model Context Protocol）服务，保存到「连接器 → MCP」列表并立即探测可用性。\
stdio 模式填 command/args；http 或 sse 模式填 url，可选 headers。",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "连接器显示名称，如 filesystem"},
                        "transport": {"type": "string", "enum": ["stdio", "http", "sse"], "description": "传输方式，默认 stdio"},
                        "command": {"type": "string", "description": "stdio 模式的启动命令，如 npx"},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "stdio 模式的命令参数"},
                        "url": {"type": "string", "description": "http/sse 模式的 MCP 地址"},
                        "headers": {"type": "object", "description": "http/sse 模式的自定义请求头"},
                        "env": {"type": "object", "description": "stdio 模式的环境变量"}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
            ),
        }
    }

    fn map_of(v: Option<&Value>) -> HashMap<String, String> {
        v.and_then(|x| x.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl AgentTool for ConnectMcp {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "ConnectMcp"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let name = str_of(&params, "name", "").trim().to_string();
        if name.is_empty() {
            return Err(AgentError::tool("缺少连接器名称 name"));
        }
        let transport = {
            let t = str_of(&params, "transport", "stdio");
            if t.is_empty() { "stdio".to_string() } else { t }
        };
        let args = params
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let cfg = McpServerConfig {
            name: name.clone(),
            transport,
            command: str_of(&params, "command", ""),
            args,
            env: Self::map_of(params.get("env")),
            url: str_of(&params, "url", ""),
            headers: Self::map_of(params.get("headers")),
            enabled: true,
        };
        if let Err(e) = self.core.mcp.upsert(cfg.clone()).await {
            return Err(AgentError::tool(format!("保存 MCP 配置失败: {e}")));
        }
        // 无论探测结果如何都通知前端刷新列表，让用户看到新连接器
        self.core.emit(Event::McpUpdated);
        let status = self.core.mcp.probe(&cfg).await;
        if !status.connected {
            return Err(AgentError::tool(format!(
                "已保存 MCP「{name}」，但连接失败：{}",
                status.error.unwrap_or_else(|| "未知错误".into())
            )));
        }
        let tools = status
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(AgentToolResult::text(format!(
            "已连接 MCP「{name}」，共 {} 个工具：{}。\n这些工具已注册为 mcp__{name}__<工具名>，下一次请求即可直接调用。",
            status.tools.len(),
            if tools.is_empty() { "（无）" } else { &tools }
        )))
    }
}

// ============================================================
// McpTool —— 把已连接 MCP 服务的工具注册为 Agent 可直接调用的工具
// 名称规约：mcp__<服务名>__<工具名>（非法字符替换为 _，截断到 64）
// ============================================================

pub struct McpTool {
    core: Arc<CoreState>,
    server: String,
    tool_name: String,
    schema: Tool,
}

impl McpTool {
    pub fn new(core: Arc<CoreState>, server: &str, info: &crate::mcp::McpToolInfo) -> Self {
        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        let mut name = format!("mcp__{}__{}", sanitize(server), sanitize(&info.name));
        name.truncate(64);
        let schema = make_tool(
            &name,
            &format!("[MCP:{}] {}", server, info.description),
            info.input_schema.clone(),
        );
        Self {
            core,
            server: server.to_string(),
            tool_name: info.name.clone(),
            schema,
        }
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let result = tokio::select! {
            r = self.core.mcp.call(&self.server, &self.tool_name, params) => r,
            _ = signal.cancelled() => return Err(AgentError::Abort),
        };
        match result {
            Ok(text) => Ok(AgentToolResult::text(text)),
            Err(e) => Err(AgentError::tool(format!(
                "MCP「{}」工具 {} 调用失败: {e}",
                self.server, self.tool_name
            ))),
        }
    }
}

// ============================================================
// DeliverFiles —— AI 主动登记本轮交付物（前端据此渲染"交付卡片"）
// ============================================================

pub struct DeliverFiles {
    schema: Tool,
}

impl Default for DeliverFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl DeliverFiles {
    pub fn new() -> Self {
        Self {
            schema: make_tool(
                "DeliverFiles",
                "把本轮交付给用户的成品文件登记为交付物清单，用于在回复区生成交付卡片。\
本轮产出或修改了文件、或需要把结果以文件形式交给用户时，必须调用本工具登记（绝对路径 + 一句话说明）。",
                json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "卡片标题，如「已编辑 8 个文件」；缺省时前端用「已交付 N 个文件」"},
                        "files": {
                            "type": "array",
                            "description": "交付物文件列表",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string", "description": "文件绝对路径"},
                                    "title": {"type": "string", "description": "显示名（缺省用文件名）"},
                                    "desc": {"type": "string", "description": "一句话说明"}
                                },
                                "required": ["path"]
                            }
                        }
                    },
                    "required": ["files"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

#[async_trait]
impl AgentTool for DeliverFiles {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "DeliverFiles"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let files = params
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            return Err(AgentError::tool(
                "files 为空：请提供至少一个交付物 {path, title?, desc?}",
            ));
        }
        let n = files.len();
        let names: Vec<String> = files
            .iter()
            .take(8)
            .map(|f| {
                let p = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
                f.get("title")
                    .and_then(|v| v.as_str())
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| p.rsplit('/').next().unwrap_or(p).to_string())
            })
            .collect();
        Ok(AgentToolResult::text(format!(
            "已交付 {} 个文件：{}",
            n,
            names.join("、")
        )))
    }
}

// ============================================================
// AskUser —— 中途播报问题并录制用户语音回复（可在设置中关闭）
// ============================================================

pub struct AskUserTool {
    core: Arc<CoreState>,
    schema: Tool,
}

impl AskUserTool {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: make_tool(
                "AskUser",
                "向用户口头提问并聆听语音回答。只有确实需要用户语音确认/补充信息时使用。",
                json!({
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "要播报给用户的问题（口语化、简短）"}
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

#[async_trait]
impl AgentTool for AskUserTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "AskUser"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let question = str_of(&params, "question", "");
        if question.trim().is_empty() {
            return Err(AgentError::tool("缺少问题 question"));
        }
        let core = self.core.clone();
        // 1) 用当前音色/语速播报问题
        let (voice, rate) = {
            let cfg = core.config.lock().await;
            (cfg.effective_voice(), cfg.rate)
        };
        let _ = core.voice.speak(question.trim(), &voice, rate).await;
        // 2) 播「开始录音」提示音，播完再静音
        {
            let beep_path = crate::paths::resource("computer_beep_1.mp3")
                .to_string_lossy()
                .into_owned();
            let _ = core.voice.beep(Some(&beep_path)).await;
        }
        // 3) 静音系统输出（避免 TTS 回声/其他声音干扰识别），识别后恢复。
        //    用 Drop guard 恢复：本轮被取消/超时提前返回时也不会把系统留在静音。
        system_mute(true);
        struct RestoreOnDrop;
        impl Drop for RestoreOnDrop {
            fn drop(&mut self) {
                system_mute(false);
            }
        }
        let _restore = RestoreOnDrop;
        let result = tokio::select! {
            r = core.voice.listen_once(15.0) => r,
            _ = _signal.cancelled() => {
                return Err(AgentError::Abort);
            }
        };
        drop(_restore);
        // 4) 播「结束录音」提示音（收到回复），异步不阻塞返回
        {
            let beep_path = crate::paths::resource("complete.mp3")
                .to_string_lossy()
                .into_owned();
            let v = core.voice.clone();
            tokio::spawn(async move {
                let _ = v.beep(Some(&beep_path)).await;
            });
        }
        // 5) 返回用户的语音回复
        match result {
            Ok(text) if !text.trim().is_empty() => Ok(AgentToolResult::text(format!(
                "用户语音回复：{}",
                text.trim()
            ))),
            Ok(_) => Err(AgentError::tool("未能听清用户的语音回复")),
            Err(e) => Err(AgentError::tool(format!("语音识别失败: {e}"))),
        }
    }
}

/// 系统级静音/恢复（macOS）：不改变音量值，恢复时回到原音量。
fn system_mute(muted: bool) {
    // 统一走 agents 里的实现，确保静音标记与启动恢复逻辑只有一份。
    crate::agents::system_mute(muted);
}
