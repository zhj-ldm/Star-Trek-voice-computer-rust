//! 子 Agent：主 Agent 在后台派发的独立任务。
//!
//! 复用 Pi 的模型通道、流式事件与工具执行逻辑，但使用独立的上下文与工具集，
//! 不占用主对话的轮次；完成后通过 SSE 事件 + 会话卡片通知 UI，并播放音效与语音。
//! 任务与执行轨迹持久化到 data/subagents.json，重启后仍可查看/按时间清理。

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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 子 Agent 的一条执行轨迹（详情页按顺序展示思考 / 工具 / 回答）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    /// user | assistant | thinking | tool
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default)]
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub progress: String,
    /// 完成摘要
    #[serde(default)]
    pub summary: String,
    pub started_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    /// 执行轨迹（详情页数据源，只保留最近若干条）
    #[serde(default)]
    pub trace: Vec<TraceEntry>,
}

#[derive(Default)]
pub struct SubagentManager {
    tasks: Mutex<Vec<SubagentTask>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    /// 完成后的模型上下文：用于用户/主 Agent 继续下发指令时续跑
    contexts: Mutex<HashMap<String, Vec<Message>>>,
    /// 运行中任务的待注入指令（在下一轮开始时并入上下文）
    inboxes: Mutex<HashMap<String, Vec<String>>>,
    /// 持久化文件（data/subagents.json）
    path: Mutex<Option<PathBuf>>,
    seq: AtomicUsize,
}

impl SubagentManager {
    /// 加载历史任务；上次退出时仍在运行的任务标记为中断。
    pub async fn init(&self, path: PathBuf) {
        *self.path.lock().await = Some(path.clone());
        let Ok(s) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(mut tasks) = serde_json::from_str::<Vec<SubagentTask>>(&s) else {
            tracing::warn!("subagents.json 解析失败，忽略历史任务");
            return;
        };
        let mut changed = false;
        for t in &mut tasks {
            if t.status == "running" {
                t.status = "failed".into();
                t.progress = "应用重启，任务已中断".into();
                t.summary = "应用重启导致该任务中断，可重新派发。".into();
                t.finished_at = Some(now_ms());
                t.updated_at = now_ms();
                changed = true;
            }
            if t.updated_at == 0 {
                t.updated_at = t.started_at;
            }
        }
        {
            let mut guard = self.tasks.lock().await;
            guard.extend(tasks);
            let mut sorted = guard.clone();
            sorted.sort_by_key(|t| t.started_at);
            *guard = sorted;
        }
        if changed {
            self.persist().await;
        }
    }

    /// 将任务列表写入磁盘（不含运行时上下文）。
    async fn persist(&self) {
        let path = self.path.lock().await.clone();
        let Some(path) = path else { return };
        let tasks = self.tasks.lock().await.clone();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&tasks) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&path, s) {
                    tracing::warn!("保存 subagents.json 失败: {e}");
                }
            }
            Err(e) => tracing::warn!("序列化 subagents 失败: {e}"),
        }
    }
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
        {
            let mut tasks = self.tasks.lock().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                if t.status == "running" {
                    t.progress = progress.to_string();
                    t.updated_at = now_ms();
                }
            }
        }
        self.persist().await;
    }

    async fn push_trace(&self, id: &str, entry: TraceEntry) {
        {
            let mut tasks = self.tasks.lock().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                t.trace.push(entry);
                // 轨迹只保留最近 300 条，避免长时间任务无限膨胀
                if t.trace.len() > 300 {
                    let cut = t.trace.len() - 300;
                    t.trace.drain(0..cut);
                }
                t.updated_at = now_ms();
            }
        }
        self.persist().await;
    }

    async fn save_context(&self, id: &str, messages: Vec<Message>) {
        self.contexts.lock().await.insert(id.to_string(), messages);
    }

    /// 给运行中的任务追加指令；返回 true 表示已排队。
    /// 任务已结束时用保存的上下文续跑，返回 false。
    pub async fn send_instruction(
        self: &Arc<Self>,
        core: Arc<CoreState>,
        id: &str,
        text: &str,
    ) -> Result<bool, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("指令不能为空".into());
        }
        let task = self.get(id).await.ok_or("任务不存在")?;
        if task.status == "running" {
            self.inboxes
                .lock()
                .await
                .entry(id.to_string())
                .or_default()
                .push(text.to_string());
            record_trace(&core, self, id, &task.parent_session, user_entry(text)).await;
            return Ok(true);
        }
        let mut messages = self
            .contexts
            .lock()
            .await
            .get(id)
            .cloned()
            .unwrap_or_else(|| {
                // 应用重启后上下文已丢失：用原任务 + 完成摘要重建一个最小上下文
                let mut m = vec![user_msg(&task.prompt)];
                if !task.summary.is_empty() {
                    m.push(user_msg(&format!("（任务此前的结论）{}", task.summary)));
                }
                m
            });
        messages.push(user_msg(text));
        let token = CancellationToken::new();
        {
            let mut tasks = self.tasks.lock().await;
            let t = tasks.iter_mut().find(|t| t.id == id).ok_or("任务不存在")?;
            t.status = "running".into();
            t.progress = "继续执行…".into();
            t.finished_at = None;
            t.updated_at = now_ms();
        }
        self.persist().await;
        self.cancels.lock().await.insert(id.to_string(), token.clone());
        record_trace(&core, self, id, &task.parent_session, user_entry(text)).await;
        core.emit(Event::SubagentProgress {
            id: id.to_string(),
            session_id: task.parent_session.clone(),
            progress: "继续执行…".into(),
        });
        let manager = self.clone();
        let core2 = core.clone();
        let id2 = id.to_string();
        let run_task_ref = self.get(id).await.ok_or("任务不存在")?;
        tokio::spawn(async move {
            run_task_loop(core2, manager, id2, run_task_ref, messages, token).await;
        });
        Ok(false)
    }

    /// 删除任务记录（可同时删除对话卡片）。运行中的任务会先被取消。
    pub async fn delete(&self, core: &Arc<CoreState>, id: &str, with_conversation: bool) -> bool {
        let existed = {
            let mut tasks = self.tasks.lock().await;
            let Some(pos) = tasks.iter().position(|t| t.id == id) else {
                return false;
            };
            tasks.remove(pos);
            true
        };
        if existed {
            if let Some(token) = self.cancels.lock().await.remove(id) {
                token.cancel();
            }
            self.contexts.lock().await.remove(id);
            self.inboxes.lock().await.remove(id);
            if with_conversation {
                let mut sessions = core.sessions.lock().await;
                sessions.remove_subagent_card(id);
            }
            core.emit(Event::SubagentDeleted {
                id: id.to_string(),
                session_id: String::new(),
                with_conversation,
            });
        }
        if existed {
            self.persist().await;
        }
        existed
    }

    /// 按时间批量清理历史记录：删除 started_at < before_ms 的已结束任务。
    /// before_ms <= 0 表示删除全部已结束任务；运行中的任务始终保留。
    /// 返回被删除的任务 id 列表。
    pub async fn purge(
        &self,
        core: &Arc<CoreState>,
        before_ms: i64,
        with_conversation: bool,
    ) -> Vec<String> {
        let mut removed = Vec::new();
        {
            let mut tasks = self.tasks.lock().await;
            tasks.retain(|t| {
                let old = before_ms <= 0 || t.started_at < before_ms;
                if t.status != "running" && old {
                    removed.push(t.id.clone());
                    false
                } else {
                    true
                }
            });
        }
        for id in &removed {
            self.contexts.lock().await.remove(id);
            self.inboxes.lock().await.remove(id);
            self.cancels.lock().await.remove(id);
            if with_conversation {
                let mut sessions = core.sessions.lock().await;
                sessions.remove_subagent_card(id);
            }
            core.emit(Event::SubagentDeleted {
                id: id.clone(),
                session_id: String::new(),
                with_conversation,
            });
        }
        if !removed.is_empty() {
            self.persist().await;
        }
        removed
    }

    async fn finish(&self, id: &str, status: &str, summary: &str) -> Option<SubagentTask> {
        let done = {
            let mut tasks = self.tasks.lock().await;
            let t = tasks.iter_mut().find(|t| t.id == id)?;
            if t.status != "running" {
                return Some(t.clone());
            }
            t.status = status.to_string();
            t.summary = summary.to_string();
            t.progress = String::new();
            t.finished_at = Some(now_ms());
            t.updated_at = now_ms();
            t.clone()
        };
        self.cancels.lock().await.remove(id);
        self.persist().await;
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
            prompt: prompt.clone(),
            status: "running".into(),
            progress: "正在准备…".into(),
            summary: String::new(),
            started_at: now_ms(),
            updated_at: now_ms(),
            finished_at: None,
            trace: vec![user_entry(&prompt)],
        };
        self.tasks.lock().await.push(task.clone());
        self.persist().await;
        let token = CancellationToken::new();
        self.cancels.lock().await.insert(id.clone(), token.clone());
        core.emit(Event::SubagentStarted {
            id: id.clone(),
            session_id: parent_session.clone(),
            session_title: task.session_title.clone(),
            title: task.title.clone(),
            prompt: task.prompt.clone(),
        });
        core.emit(Event::SubagentTrace {
            id: id.clone(),
            session_id: parent_session.clone(),
            entry: serde_json::to_value(user_entry(&prompt)).unwrap_or(Value::Null),
        });
        let manager = self.clone();
        let run_task_ref = task.clone();
        let messages = vec![user_msg(&prompt)];
        tokio::spawn(async move {
            run_task_loop(core, manager, id, run_task_ref, messages, token).await;
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
/// messages 为本次运行的初始上下文（首次派发 = prompt；继续执行 = 历史上下文 + 新指令）。
async fn run_task_loop(
    core: Arc<CoreState>,
    manager: Arc<SubagentManager>,
    id: String,
    task: SubagentTask,
    mut messages: Vec<Message>,
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
        // 运行中收到的追加指令：在下一轮开始时并入上下文。
        let pending = manager
            .inboxes
            .lock()
            .await
            .remove(&id)
            .unwrap_or_default();
        if !pending.is_empty() {
            for ins in pending {
                messages.push(user_msg(&ins));
            }
            manager.update_progress(&id, "正在处理新指令…").await;
            core.emit(Event::SubagentProgress {
                id: id.clone(),
                session_id: task.parent_session.clone(),
                progress: "正在处理新指令…".into(),
            });
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
        let mut round_thinking = String::new();
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
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    if !delta.is_empty() {
                        round_thinking.push_str(&delta);
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
        if !round_thinking.trim().is_empty() {
            record_trace(
                &core,
                &manager,
                &id,
                &task.parent_session,
                text_entry("thinking", &trim_chars(&round_thinking, 2000)),
            )
            .await;
        }
        if !round_text.trim().is_empty() {
            record_trace(
                &core,
                &manager,
                &id,
                &task.parent_session,
                text_entry("assistant", &trim_chars(&round_text, 4000)),
            )
            .await;
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
            record_trace(
                &core,
                &manager,
                &id,
                &task.parent_session,
                TraceEntry {
                    kind: "tool".into(),
                    text: trim_chars(&text, 2000),
                    name: Some(call.name.clone()),
                    input: Some(call.arguments.clone()),
                    ok: Some(ok),
                    at: now_ms(),
                },
            )
            .await;
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
    // 保留完整上下文，便于用户/主 Agent 在详情页继续下发指令。
    manager.save_context(&id, messages).await;
    finish_and_notify(&core, &manager, &id, &task, status, &summary).await;
}

fn user_entry(text: &str) -> TraceEntry {
    text_entry("user", text)
}

fn text_entry(kind: &str, text: &str) -> TraceEntry {
    TraceEntry {
        kind: kind.into(),
        text: text.into(),
        name: None,
        input: None,
        ok: None,
        at: now_ms(),
    }
}

/// 记录一条轨迹并实时推给前端；任务已被删除时静默丢弃。
async fn record_trace(
    core: &Arc<CoreState>,
    manager: &Arc<SubagentManager>,
    id: &str,
    session_id: &str,
    entry: TraceEntry,
) {
    manager.push_trace(id, entry.clone()).await;
    core.emit(Event::SubagentTrace {
        id: id.to_string(),
        session_id: session_id.to_string(),
        entry: serde_json::to_value(&entry).unwrap_or(Value::Null),
    });
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

/// 收尾：更新任务、写入会话卡片、SSE 通知、音效，然后唤醒主 AI 生成最终汇报。
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

    // 完成音效（短促提示）：随后由主 AI 用自然语言向用户汇报并语音播报。
    let effect = crate::paths::resource("subagent_done.mp3");
    if effect.exists() {
        let effect_path = effect.to_string_lossy().into_owned();
        let _ = core.voice.beep(Some(effect_path.as_str())).await;
    }
    // 取消是用户主动行为，不再唤醒主 AI；完成/失败都要让主 AI 汇报。
    if status == "cancelled" {
        return;
    }
    let core2 = core.clone();
    let parent = task.parent_session.clone();
    let status_cn = if ok { "已完成" } else { "执行失败" };
    // 唤醒主 AI：等它空闲且不在播报后，把汇报作为一次「汇报轮」注入，
    // 回复走正常对话渲染与 TTS，等同用户发消息后的效果。
    let report = format!(
        "后台子 Agent「{}」{}。\n结果摘要：{}\n\n请直接用一两句自然、口语化的中文向用户汇报这个结果，不要调用工具，不要重复罗列细节。",
        done.title,
        status_cn,
        trim_chars(summary, 500)
    );
    tokio::spawn(async move {
        // 最多等待 3 分钟，避免主 AI 长时间忙碌时汇报永远发不出去。
        for _ in 0..1800 {
            if !core2.main_busy.load(Ordering::SeqCst) && !core2.speaking.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        crate::agents::run_main_turn(core2, report, Some(parent), true).await;
    });
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
- 用户要给运行中的任务补充要求、或要打断后继续：调用 SendSubagentMessage（id + message）。
  运行中会在下一轮生效；已完成的任务会自动带着原上下文继续跑。
- 用户要删除任务记录：调用 DeleteSubagent（id，或 before_days 按时间批量清理）。
  with_conversation=true 时同时删除主对话里的完成卡片。
- 子 Agent 完成后系统会自动唤醒你，由你用一两句话向用户汇报；不要长篇复述它的结果。

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

pub struct SendSubagentMessage {
    core: Arc<CoreState>,
    schema: rpi_ai::types::Tool,
}

impl SendSubagentMessage {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: rpi_ai::types::Tool {
                name: "SendSubagentMessage".into(),
                description: "给子 Agent 追加指令：运行中的任务会在下一轮执行；\
已结束的任务会带着原上下文继续执行。用于打断后补充要求或继续对话。"
                    .into(),
                parameters: rpi_ai::types::Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "子 Agent 任务 id"},
                        "message": {"type": "string", "description": "要追加给子 Agent 的指令或消息"}
                    },
                    "required": ["id", "message"],
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for SendSubagentMessage {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "SendSubagentMessage"
    }
    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if id.is_empty() || message.is_empty() {
            return Err(AgentError::tool("id 与 message 都不能为空"));
        }
        match self
            .core
            .subagents
            .send_instruction(self.core.clone(), id, message)
            .await
        {
            Ok(true) => Ok(rpi_agent::types::AgentToolResult::text(format!(
                "指令已加入子 Agent {id} 的待处理队列，下一轮开始生效。"
            ))),
            Ok(false) => Ok(rpi_agent::types::AgentToolResult::text(format!(
                "子 Agent {id} 已带着原上下文继续执行。"
            ))),
            Err(e) => Err(AgentError::tool(e)),
        }
    }
}

pub struct DeleteSubagent {
    core: Arc<CoreState>,
    schema: rpi_ai::types::Tool,
}

impl DeleteSubagent {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self {
            core,
            schema: rpi_ai::types::Tool {
                name: "DeleteSubagent".into(),
                description: "删除子 Agent 任务记录。可指定 id 删除单条，或按 before_days \
批量清理已结束的旧记录；运行中的任务不会被清理。".into(),
                parameters: rpi_ai::types::Schema::new(json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "要删除的任务 id（可选）"},
                        "before_days": {"type": "number", "description": "删除多少天前的记录；0 = 全部已结束记录"},
                        "with_conversation": {"type": "boolean", "description": "是否同时删除主对话里的完成卡片"}
                    },
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait]
impl AgentTool for DeleteSubagent {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "DeleteSubagent"
    }
    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::types::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
        let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let with_conversation = params
            .get("with_conversation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !id.is_empty() {
            let ok = self
                .core
                .subagents
                .delete(&self.core, id, with_conversation)
                .await;
            return Ok(rpi_agent::types::AgentToolResult::text(if ok {
                format!("已删除子 Agent 任务 {id}。")
            } else {
                format!("没有找到子 Agent 任务 {id}。")
            }));
        }
        let days = params
            .get("before_days")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let before = if days > 0.0 {
            now_ms() - (days * 86_400_000.0) as i64
        } else {
            0
        };
        let removed = self
            .core
            .subagents
            .purge(&self.core, before, with_conversation)
            .await;
        Ok(rpi_agent::types::AgentToolResult::text(format!(
            "已清理 {} 条子 Agent 历史记录。",
            removed.len()
        )))
    }
}
