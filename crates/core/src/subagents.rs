//! 子 Agent：主 Agent 在后台派发的独立任务。
//!
//! 复用 Pi 的模型通道、流式事件与工具执行逻辑，但使用独立的上下文与工具集，
//! 不占用主对话的轮次；完成后通过 SSE 事件 + 会话卡片通知 UI，并播放音效与语音。
//! 任务只保存在内存中（重启即清空），避免后台任务污染磁盘历史。

use crate::agents::{build_llm_parts, Channel};
use crate::events::Event;
use crate::state::CoreState;
use crate::tools::child_tools;
use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_ai::types::{
    AssistantMessageEvent, Content, Context, Message, StopReason, ToolCall, ToolResultMessage,
    ToolResultRole, UserMessage,
};
use rpi_ai::{Provider, SimpleStreamOptions};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize)]
pub struct SubagentTask {
    pub id: String,
    pub parent_session: String,
    pub session_title: String,
    /// 任务内容（简短标题）
    pub title: String,
    /// 完整任务说明
    pub prompt: String,
    /// running | done | failed | cancelled
    pub status: String,
    /// 最近一次进度（如「正在执行 Bash」）
    pub progress: String,
    /// 完成摘要
    pub summary: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Default)]
pub struct SubagentManager {
    tasks: Mutex<Vec<SubagentTask>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    seq: AtomicUsize,
}

impl SubagentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn list(&self) -> Vec<SubagentTask> {
        let mut out = self.tasks.lock().await.clone();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    pub async fn get(&self, id: &str) -> Option<SubagentTask> {
        self.tasks.lock().await.iter().find(|t| t.id == id).cloned()
    }

    /// 取消一个正在运行的任务；返回是否真的发出了取消。
    pub async fn cancel(&self, id: &str) -> bool {
        let cancels = self.cancels.lock().await;
        if let Some(token) = cancels.get(id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    async fn update_progress(&self, id: &str, progress: &str) {
        let mut tasks = self.tasks.lock().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            if t.status == "running" {
                t.progress = progress.to_string();
            }
        }
    }

    async fn finish(&self, id: &str, status: &str, summary: &str) -> Option<SubagentTask> {
        let mut tasks = self.tasks.lock().await;
        let t = tasks.iter_mut().find(|t| t.id == id)?;
        if t.status != "running" {
            return Some(t.clone());
        }
        t.status = status.to_string();
        t.summary = summary.to_string();
        t.progress = String::new();
        t.finished_at = Some(now_ms());
        let done = t.clone();
        self.cancels.lock().await.remove(id);
        Some(done)
    }

    /// 派发任务：立即返回任务 id，实际执行在后台 tokio 任务中进行。
    pub async fn spawn(
        self: &Arc<Self>,
        core: Arc<CoreState>,
        parent_session: String,
        session_title: String,
        title: String,
        prompt: String,
    ) -> SubagentTask {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("sa{}-{:x}", now_ms(), n);
        let task = SubagentTask {
            id: id.clone(),
            parent_session: parent_session.clone(),
            session_title,
            title,
            prompt,
            status: "running".into(),
            progress: "正在准备…".into(),
            summary: String::new(),
            started_at: now_ms(),
            finished_at: None,
        };
        self.tasks.lock().await.push(task.clone());
        let token = CancellationToken::new();
        self.cancels.lock().await.insert(id.clone(), token.clone());
        core.emit(Event::SubagentStarted {
            id: id.clone(),
            session_id: parent_session.clone(),
            session_title: task.session_title.clone(),
            title: task.title.clone(),
            prompt: task.prompt.clone(),
        });
        let manager = self.clone();
        let run_task_ref = task.clone();
        tokio::spawn(async move {
            run_task(core, manager, id, run_task_ref, token).await;
        });
        task
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage::new(text.to_string(), 0))
}

fn assistant_msg(m: rpi_ai::types::AssistantMessage) -> Message {
    Message::Assistant(Box::new(m))
}

fn tool_result_msg(call: &ToolCall, text: &str, is_error: bool) -> Message {
    Message::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![Content::text(text)],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error,
        timestamp: 0,
    }))
}

fn tool_result_text(r: &rpi_agent::types::AgentToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            rpi_agent::types::TextContentOrImage::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn trim_chars(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

const CHILD_SYSTEM_PROMPT: &str = "\
你是 Star Trek Computer 的后台子 Agent，由主 Agent 派发，独立完成一项具体任务。

工作规则：
1. 直接动手执行，不要向用户提问，也不要输出【Voice】播报段。
2. 优先用工具核实事实（read/write/edit/bash/WebSearch 等），文件路径一律使用绝对路径。
3. 不要调用语音、子 Agent、交付卡片等能力；这些由主 Agent 负责。
4. 任务完成后，用不超过 120 字的中文给出结果摘要：关键结论、产出文件路径、遗留问题。
5. 如果任务无法完成，直接说明原因和已尝试的步骤，不要编造结果。";

/// 子 Agent 的完整执行流程。失败/取消都会落到任务状态里，不向外 panic。
async fn run_task(
    core: Arc<CoreState>,
    manager: Arc<SubagentManager>,
    id: String,
    task: SubagentTask,
    cancel: CancellationToken,
) {
    let (channels, model, reasoning) = match build_llm_parts(&core).await {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("无法初始化模型：{e}");
            finish_and_notify(&core, &manager, &id, &task, "failed", &msg).await;
            return;
        }
    };
    let max_turns = {
        let cfg = core.config.lock().await;
        cfg.max_turns.clamp(5, 60) as usize
    };
    let tools = child_tools();
    let ctx_tools: Vec<_> = tools.iter().map(|t| t.schema().clone()).collect();
    let mut messages: Vec<Message> = vec![user_msg(&task.prompt)];
    let mut final_text = String::new();
    let mut status = "done";
    let mut error: Option<String> = None;
    let mut cursor = 0usize;
    let mut stall_retries = 0u8;

    for turn in 0..max_turns {
        if cancel.is_cancelled() {
            status = "cancelled";
            break;
        }
        let ctx = Context {
            system_prompt: Some(CHILD_SYSTEM_PROMPT.to_string()),
            messages: messages.clone(),
            tools: ctx_tools.clone(),
        };
        let ch: &Channel = &channels[cursor % channels.len()];
        cursor += 1;
        ch.wait_slot().await;
        let opts = SimpleStreamOptions {
            api_key: ch.api_key.clone(),
            max_retries: Some(2),
            signal: cancel.clone(),
            reasoning,
            ..Default::default()
        };
        let mut stream = ch.provider.stream_simple(&model, &ctx, &opts).await;

        let mut round_text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut assistant = None;
        let mut failure: Option<String> = None;
        loop {
            let ev = tokio::select! {
                _ = cancel.cancelled() => {
                    status = "cancelled";
                    break;
                }
                ev = tokio::time::timeout(Duration::from_secs(300), stream.next()) => {
                    match ev {
                        Ok(Some(e)) => e,
                        Ok(None) => break,
                        Err(_) => {
                            failure = Some("模型响应超时".into());
                            break;
                        }
                    }
                }
            };
            match ev {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    if !delta.is_empty() {
                        round_text.push_str(&delta);
                    }
                }
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    calls.push(tool_call);
                }
                AssistantMessageEvent::Done { message, .. } => {
                    if round_text.trim().is_empty() {
                        round_text = assistant_text_of(&message);
                    }
                    assistant = Some(message);
                    break;
                }
                AssistantMessageEvent::Error { error: e, .. } => {
                    if e.stop_reason == StopReason::Aborted {
                        status = "cancelled";
                    } else {
                        failure = Some(e.error_message.unwrap_or_else(|| "模型请求失败".into()));
                    }
                    break;
                }
                _ => {}
            }
        }
        if status == "cancelled" {
            break;
        }
        if let Some(e) = failure {
            error = Some(e);
            status = "failed";
            break;
        }
        // 被长度截断的助手消息不能回填上游，直接按失败处理。
        let stop_reason = assistant
            .as_ref()
            .map(|a| a.stop_reason)
            .unwrap_or(StopReason::Stop);
        if let Some(a) = assistant {
            if stop_reason != StopReason::Length && (!calls.is_empty() || !round_text.trim().is_empty())
            {
                messages.push(assistant_msg(a));
            }
        }
        if !round_text.trim().is_empty() {
            if !final_text.is_empty() {
                final_text.push_str("\n\n");
            }
            final_text.push_str(&round_text);
        }
        if calls.is_empty() {
            if round_text.trim().is_empty() && stall_retries < 2 && turn + 1 < max_turns {
                stall_retries += 1;
                messages.push(user_msg("请继续执行任务；完成后给出简短结果摘要。"));
                continue;
            }
            break;
        }

        stall_retries = 0;
        for call in calls {
            if cancel.is_cancelled() {
                status = "cancelled";
                break;
            }
            let label = format!("正在执行 {}", call.name);
            manager.update_progress(&id, &label).await;
            core.emit(Event::SubagentProgress {
                id: id.clone(),
                session_id: task.parent_session.clone(),
                progress: label,
            });
            let tool = tools.iter().find(|t| t.schema().name == call.name).cloned();
            let (text, ok) = match tool {
                Some(t) => {
                    let exec = t.execute(
                        &call.id,
                        call.arguments.clone(),
                        cancel.child_token(),
                        Arc::new(|_| {}),
                    );
                    match exec.await {
                        Ok(r) => (tool_result_text(&r), true),
                        Err(e) => (format!("工具执行失败：{e}"), false),
                    }
                }
                None => (format!("工具 {} 不可用", call.name), false),
            };
            messages.push(tool_result_msg(&call, &text, !ok));
        }
        if status == "cancelled" {
            break;
        }
        if turn + 1 == max_turns {
            error = Some(format!("已达到最大轮数 {max_turns}，任务未完成"));
            status = "failed";
        }
    }

    if cancel.is_cancelled() {
        status = "cancelled";
    }
    let summary = if status == "done" {
        let s = final_text.trim();
        if s.is_empty() {
            "任务已结束，但模型没有返回摘要。".to_string()
        } else {
            trim_chars(s, 600)
        }
    } else if status == "cancelled" {
        "任务已被取消。".to_string()
    } else {
        error.unwrap_or_else(|| "任务执行失败。".to_string())
    };
    finish_and_notify(&core, &manager, &id, &task, status, &summary).await;
}

fn assistant_text_of(m: &rpi_ai::types::AssistantMessage) -> String {
    m.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 收尾：更新任务、写入会话卡片、SSE 通知、音效 + 语音。
async fn finish_and_notify(
    core: &Arc<CoreState>,
    manager: &Arc<SubagentManager>,
    id: &str,
    task: &SubagentTask,
    status: &str,
    summary: &str,
) {
    let Some(done) = manager.finish(id, status, summary).await else {
        return;
    };
    let ok = status == "done";
    let card_text = format!(
        "子 Agent「{}」{}：{}",
        done.title,
        if ok { "已完成" } else if status == "cancelled" { "已取消" } else { "执行失败" },
        summary
    );
    {
        let mut sessions = core.sessions.lock().await;
        sessions.append_subagent(
            &task.parent_session,
            &done.title,
            &card_text,
            summary,
            ok,
            status,
            &done.id,
        );
    }
    core.emit(Event::SubagentDone {
        id: done.id.clone(),
        session_id: task.parent_session.clone(),
        session_title: task.session_title.clone(),
        title: done.title.clone(),
        summary: summary.to_string(),
        ok,
        status: status.to_string(),
    });

    // 音效 + 语音：先播放桌面上指定的完成音效，再朗读简短摘要。
    let effect = crate::paths::resource("subagent_done.mp3");
    if effect.exists() {
        let effect_path = effect.to_string_lossy().into_owned();
        let _ = core.voice.beep(Some(effect_path.as_str())).await;
    }
    let (enabled, voice, rate) = {
        let cfg = core.config.lock().await;
        (cfg.voice_enabled, cfg.voice.clone(), cfg.rate)
    };
    if enabled && ok {
        let speak = format!("子 Agent 已完成：{}。{}", done.title, trim_chars(summary, 60));
        let _ = core.voice.speak(&speak, &voice, rate).await;
    }
}

// ============================================================
// 主 Agent 可调用的工具
// ============================================================

fn guide_text() -> String {
    "\
子 Agent 执行细则（派发前必读）

一、何时派发
- 只有任务复杂、耗时长（预计超过数分钟）、需要大量检索/读写文件/反复试错，且会明显阻塞当前对话时，才派发。
- 简单问答、单步修改、几十行代码、一次搜索即可完成的任务，自己处理，不要派发。
- 同一任务不要重复派发；派发前可先用 ListSubagents 查看是否已有同类任务在跑。

二、派发方式
- 调用 DispatchSubagent，参数：
  · title：不超过 20 字的任务名（用于列表与通知卡片）
  · prompt：完整、可独立执行的指令。必须写清目标、输入材料、约束、验收标准；
    子 Agent 看不到当前对话上下文，也不会再向你或用户提问。
- 派发成功后立即向用户简短说明已派发，然后继续处理其他可同步进行的工作；
  如果没有别的任务，告诉用户「请稍等」。

三、运行期
- 用户询问进度时调用 ListSubagents，用一两句话汇报（任务名 + 当前进度）。
- 用户要求停止/打断某个任务时调用 CancelSubagent（传 id；或 all=true 取消全部）。
- 子 Agent 完成会自动以独立卡片通知用户并语音播报，你不要再长篇复述它的结果。

四、边界
- 不要在 prompt 中要求子 Agent 派发子 Agent、播放语音或生成交付卡片。
- 子 Agent 只能读写本机文件、执行命令、搜索网页；需要用户决策的事情必须由主 Agent 处理。"
        .to_string()
}

pub struct GetSubagentGuide;

#[async_trait]
impl AgentTool for GetSubagentGuide {
    fn schema(&self) -> &rpi_ai::types::Tool {
        static SCHEMA: std::sync::OnceLock<rpi_ai::types::Tool> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| rpi_ai::types::Tool {
            name: "GetSubagentGuide".into(),
            description: "获取子 Agent 的详细执行细则。派发子 Agent 之前必须先调用本工具。".into(),
            parameters: rpi_ai::types::Schema::new(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            constrained_sampling: None,
        })
    }
    fn label(&self) -> &str {
        "GetSubagentGuide"
    }
    async fn execute(
        &self,
        _id: &str,
        _params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        Ok(rpi_agent::types::AgentToolResult::text(guide_text()))
    }
}

pub struct DispatchSubagent {
    core: Arc<CoreState>,
    session_id: String,
    session_title: String,
    schema: rpi_ai::types::Tool,
}

impl DispatchSubagent {
    pub fn new(core: Arc<CoreState>, session_id: String, session_title: String) -> Self {
        Self {
            core,
            session_id,
            session_title,
            schema: rpi_ai::types::Tool {
                name: "DispatchSubagent".into(),
                description: "把一个复杂、耗时、可独立执行的任务派发给后台子 Agent。\
必须先调用 GetSubagentGuide 了解细则。派发后立即返回任务 id，不等待结果。"
                    .into(),
                parameters: rpi_ai::types::Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "不超过 20 字的任务名"},
                        "prompt": {"type": "string", "description": "完整可独立执行的任务说明：目标、材料、约束、验收标准"}
                    },
                    "required": ["title", "prompt"],
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for DispatchSubagent {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "DispatchSubagent"
    }
    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() || prompt.is_empty() {
            return Err(AgentError::tool("title 与 prompt 都不能为空"));
        }
        let task = self
            .core
            .subagents
            .spawn(
                self.core.clone(),
                self.session_id.clone(),
                self.session_title.clone(),
                trim_chars(&title, 40),
                prompt,
            )
            .await;
        Ok(rpi_agent::types::AgentToolResult::text(format!(
            "已派发子 Agent：{}（id={}）。完成后会自动通知用户，你可以继续处理其他事情。",
            task.title, task.id
        )))
    }
}

pub struct ListSubagents {
    core: Arc<CoreState>,
    schema: rpi_ai::types::Tool,
}

impl ListSubagents {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: rpi_ai::types::Tool {
                name: "ListSubagents".into(),
                description: "列出所有子 Agent 任务及状态/进度，用于回答用户对进度的询问。".into(),
                parameters: rpi_ai::types::Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "only_running": {"type": "boolean", "description": "只看运行中的任务"}
                    },
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for ListSubagents {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "ListSubagents"
    }
    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        let only_running = params
            .get("only_running")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tasks = self.core.subagents.list().await;
        let tasks: Vec<_> = tasks
            .into_iter()
            .filter(|t| !only_running || t.status == "running")
            .collect();
        if tasks.is_empty() {
            return Ok(rpi_agent::types::AgentToolResult::text("当前没有子 Agent 任务。"));
        }
        let mut out = String::from("子 Agent 任务：\n");
        for t in tasks {
            out.push_str(&format!(
                "- [{}] {}（对话：{}）{} {}\n",
                t.status,
                t.title,
                t.session_title,
                if t.status == "running" {
                    format!("进度：{}", t.progress)
                } else {
                    format!("摘要：{}", trim_chars(&t.summary, 120))
                },
                format!("id={}", t.id)
            ));
        }
        Ok(rpi_agent::types::AgentToolResult::text(out))
    }
}

pub struct CancelSubagent {
    core: Arc<CoreState>,
    schema: rpi_ai::types::Tool,
}

impl CancelSubagent {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: rpi_ai::types::Tool {
                name: "CancelSubagent".into(),
                description: "中止一个正在运行的子 Agent 任务（用户要求停止/打断时使用）。".into(),
                parameters: rpi_ai::types::Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "任务 id；留空则需 all=true"},
                        "all": {"type": "boolean", "description": "是否取消全部运行中的任务"}
                    },
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for CancelSubagent {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "CancelSubagent"
    }
    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        let all = params.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if all {
            let tasks = self.core.subagents.list().await;
            let mut n = 0;
            for t in tasks.iter().filter(|t| t.status == "running") {
                if self.core.subagents.cancel(&t.id).await {
                    n += 1;
                }
            }
            return Ok(rpi_agent::types::AgentToolResult::text(format!(
                "已请求取消 {n} 个运行中的子 Agent。"
            )));
        }
        if id.is_empty() {
            return Err(AgentError::tool("请提供要取消的任务 id，或传 all=true"));
        }
        if self.core.subagents.cancel(id).await {
            Ok(rpi_agent::types::AgentToolResult::text(format!(
                "已请求取消子 Agent {id}。"
            )))
        } else {
            Ok(rpi_agent::types::AgentToolResult::text(format!(
                "没有找到正在运行的子 Agent {id}。"
            )))
        }
    }
}
