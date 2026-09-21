//! Agent 编排：构建主/子 Agent、主 Agent 会话轮、子 Agent 任务执行、
//! 唤醒词处理与自动语音汇报。主 Agent 只开放搜索+派发+监控+语音工具。

use crate::events::Event;
use crate::sessions::ToolCallRecord;
use crate::search::WebSearchTool;
use crate::state::CoreState;
use crate::tools::{DispatchTask, ImportSkill, MonitorSubagent, SpeakToUser, StopSubagent};
use open_agent_sdk::utils::messages::{create_assistant_message, create_user_message};
use open_agent_sdk::{Agent, AgentOptions, ContentBlock, Message, SDKMessage};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub type MainAgent = Agent;
pub type SubAgent = Agent;

// ============================================================
// 构建 Agent
// ============================================================

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users/zhj".into())
}

/// 构建主 Agent：仅开放 搜索 + 派发/监控子任务 + 语音播报/导入 skill
async fn build_main_agent(core: Arc<CoreState>) -> Result<MainAgent, String> {
    let cfg = core.config.lock().await;
    let skills_text = core.skills.usage_text().await;
    let memory_text = core.memory.context_text(30).await;
    let append = format!(
        "\n\n=== 可用 Skills ===\n{}\n\n=== 长期记忆 ===\n{}",
        skills_text, memory_text
    );
    let mut opts = AgentOptions::default();
    opts.model = Some(cfg.main_model.clone());
    opts.api_key = Some(cfg.main_api_key.clone());
    opts.base_url = Some(cfg.main_base_url.clone());
    opts.cwd = Some(home_dir());
    opts.system_prompt = Some(cfg.main_system_prompt.clone());
    opts.append_system_prompt = Some(append);
    opts.max_turns = Some(cfg.max_turns);
    opts.custom_tools = vec![
        Arc::new(SpeakToUser::new(core.clone())),
        Arc::new(DispatchTask::new(core.clone())),
        Arc::new(MonitorSubagent::new(core.clone())),
        Arc::new(StopSubagent::new(core.clone())),
        Arc::new(ImportSkill::new(core.clone())),
        Arc::new(WebSearchTool::default()),
    ];
    opts.allowed_tools = Some(vec![
        "WebSearch".into(),
        "SpeakToUser".into(),
        "DispatchTask".into(),
        "MonitorSubagent".into(),
        "StopSubagent".into(),
        "ImportSkill".into(),
    ]);
    Agent::new(opts).await
}

/// 构建子 Agent：完整工具能力
async fn build_sub_agent(core: Arc<CoreState>) -> Result<SubAgent, String> {
    let cfg = core.config.lock().await;
    let skills_text = core.skills.usage_text().await;
    let memory_text = core.memory.context_text(20).await;
    let append = format!(
        "\n\n=== 可用 Skills ===\n{}\n\n=== 长期记忆 ===\n{}",
        skills_text, memory_text
    );
    let mut opts = AgentOptions::default();
    opts.model = Some(cfg.sub_model.clone());
    opts.api_key = Some(cfg.sub_api_key.clone());
    opts.base_url = Some(cfg.sub_base_url.clone());
    opts.cwd = Some(home_dir());
    opts.system_prompt = Some(cfg.sub_system_prompt.clone());
    opts.append_system_prompt = Some(append);
    opts.max_turns = Some(cfg.max_turns);
    // 覆盖占位 WebSearch 为真实实现
    opts.custom_tools = vec![Arc::new(WebSearchTool::default())];
    Agent::new(opts).await
}

/// 确保双 Agent 已构建（API 配置就绪）
pub async fn ensure_agents(core: &Arc<CoreState>) -> Result<(), String> {
    if core.agents_ready.load(Ordering::SeqCst) {
        return Ok(());
    }
    {
        let cfg = core.config.lock().await;
        if cfg.main_api_key.is_empty() || cfg.sub_api_key.is_empty() {
            return Err("请先在设置中配置主 Agent 与子 Agent 的 API Key".into());
        }
    }
    {
        let mut mg = core.main_agent.lock().await;
        if mg.is_none() {
            *mg = Some(build_main_agent(core.clone()).await?);
        }
    }
    {
        let mut sg = core.sub_agent.lock().await;
        if sg.is_none() {
            *sg = Some(build_sub_agent(core.clone()).await?);
        }
    }
    core.agents_ready.store(true, Ordering::SeqCst);
    core.set_main_status("idle");
    core.set_sub_status("idle");
    Ok(())
}

/// 重建双 Agent（设置变更/记忆变更后调用）
pub async fn rebuild_agents(core: &Arc<CoreState>) {
    {
        let mut mg = core.main_agent.lock().await;
        *mg = None;
    }
    {
        let mut sg = core.sub_agent.lock().await;
        *sg = None;
    }
    core.agents_ready.store(false, Ordering::SeqCst);
    let _ = ensure_agents(core).await;
}

// ============================================================
// 主 Agent 会话轮
// ============================================================

pub async fn run_main_turn(
    core: Arc<CoreState>,
    user_text: String,
    session_id: Option<String>,
    is_report: bool,
) {
    if let Err(e) = ensure_agents(&core).await {
        core.emit(Event::AssistantDone {
            session_id: String::new(),
            text: format!("主 Agent 未就绪：{e}"),
        });
        return;
    }
    // 会话归属：显式 session_id 优先，否则落到 active 会话（无则新建）
    let sid = {
        let mut sessions = core.sessions.lock().await;
        let sid = match session_id {
            Some(id) if sessions.get(&id).is_some() => {
                sessions.switch(&id);
                Some(id)
            }
            _ => Some(sessions.ensure_active().id),
        };
        sid.expect("session id")
    };
    core.main_busy.store(true, Ordering::SeqCst);
    core.interrupt_main.store(false, Ordering::SeqCst);
    core.set_main_status("working");
    if is_report {
        // 子 Agent 汇报：独立事件，不以用户消息形式进入会话，也不写入用户历史
        core.emit(Event::ReportText {
            session_id: sid.clone(),
            text: user_text.clone(),
        });
    } else {
        core.emit(Event::UserText {
            session_id: sid.clone(),
            text: user_text.clone(),
        });
    }

    let mut guard = core.main_agent.lock().await;
    let agent = match guard.as_mut() {
        Some(a) => a,
        None => {
            core.main_busy.store(false, Ordering::SeqCst);
            core.set_main_status("idle");
            return;
        }
    };

    // 恢复当前会话历史到 Agent 上下文（切换会话后上下文相互独立）。
    // 注意：此时尚未 append 当前 user，恢复的历史不含当前输入，
    // 避免 query() 内部再 push 一次 user 导致消息双发。
    {
        let history = {
            let sessions = core.sessions.lock().await;
            sessions
                .recent_messages(&sid, 40)
                .into_iter()
                .map(|m| {
                    if m.role == "user" {
                        create_user_message(&m.text)
                    } else {
                        create_assistant_message(&m.text)
                    }
                })
                .collect::<Vec<Message>>()
        };
        // 无条件覆盖：新会话无历史时清空残留，避免跨会话上下文串扰
        agent.messages = history;
    }

    // 当前 user 消息持久化（历史恢复之后再写，保证恢复的历史不含当前输入）。
    // 子 Agent 汇报不入用户历史，避免污染会话（避免切换会话后把汇报当作用户指令重放）。
    if !is_report {
        let mut sessions = core.sessions.lock().await;
        sessions.append(&sid, "user", &user_text, Vec::new());
    }

    // 播放“已发送”提示音（原版语义：把用户输入发给 AI 时播放 complete.mp3）。
    // 子 Agent 汇报属自动流程，不播放该提示音。
    if !is_report {
        let complete_file = format!(
            "{}/Projects/star-trek-assistant/resources/complete.mp3",
            home_dir()
        );
        let _ = core.voice.beep(Some(&complete_file)).await;
    }

    // 本轮播报计数复位（SpeakToUser 成功调用时置位）
    core.turn_spoken.store(false, Ordering::SeqCst);
    let (mut rx, mut handle) = agent.query(&user_text).await;

    let mut _final_text = String::new();
    let mut interrupted = false;
    let mut persisted = false; // assistant 是否已在 Result 分支落盘
    // 未播报驳回重试次数（上限 2 次，防止模型不配合时死循环）
    let mut speak_retry = 0;
    // 本轮工具调用记录（随 assistant 消息持久化，供切换会话后恢复渲染）
    let mut tool_log: Vec<ToolCallRecord> = Vec::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if core.interrupt_main.load(Ordering::SeqCst) {
                    interrupted = true;
                    handle.abort();
                    break;
                }
            }
            ev = rx.recv() => {
                match ev {
                    Some(SDKMessage::PartialMessage { text }) => {
                        // 兼容两种语义：全量快照（新文本是旧文本前缀）则覆盖，增量片段则追加
                        if _final_text.starts_with(&text) {
                            _final_text = text.clone();
                        } else {
                            _final_text.push_str(&text);
                        }
                        core.emit(Event::AssistantText {
                            session_id: sid.clone(),
                            text,
                        });
                    }
                    Some(SDKMessage::Assistant { message, .. }) => {
                        for block in &message.content {
                            if let ContentBlock::ToolUse { name, input, .. } = block {
                                core.emit(Event::ToolUse {
                                    session_id: sid.clone(),
                                    agent: "main".into(),
                                    name: name.clone(),
                                    input: input.clone(),
                                });
                                tool_log.push(ToolCallRecord {
                                    name: name.clone(),
                                    input: input.clone(),
                                    ok: true,
                                    summary: String::new(),
                                });
                            } else if let ContentBlock::Thinking { thinking, .. } = block {
                                // AI 中间思考过程：前端以左箭头折叠卡展示
                                if !thinking.is_empty() {
                                    core.emit(Event::ReasoningText {
                                        session_id: sid.clone(),
                                        text: thinking.clone(),
                                    });
                                }
                            }
                        }
                        let t = extract_message_text(&message);
                        if !t.is_empty() {
                            // 每条 Assistant 消息都是当时完整文本的快照，取最新最全覆盖最终文本
                            // （Result.text 经常为空，不能作为持久化依据）。
                            // 不在中途 emit AssistantText：快照语义下前端逐条拼接会文本重复，
                            // 最终文本统一由 Result 分支一次性发出。
                            _final_text = t;
                        }
                    }
                    Some(SDKMessage::ToolResult { tool_name, content, is_error, .. }) => {
                        let summary = content.chars().take(300).collect::<String>();
                        core.emit(Event::ToolResult {
                            session_id: sid.clone(),
                            agent: "main".into(),
                            name: tool_name.clone(),
                            ok: !is_error,
                            summary: summary.clone(),
                        });
                        if let Some(rec) = tool_log.iter_mut().rev().find(|r| r.name == tool_name) {
                            rec.ok = !is_error;
                            rec.summary = summary;
                        }
                    }
                    Some(SDKMessage::Result { text, .. }) => {
                        if !text.is_empty() {
                            _final_text = text.clone();
                        }
                        // 播报硬约束：本轮必须至少调用过一次 SpeakToUser。
                        // 若模型直接结束而未播报，则驳回结束请求，把最终文本作为必须播报的内容
                        // 再次送入模型，强制其调用 SpeakToUser 完成播报后再结束（上限 3 次）。
                        if !core.turn_spoken.load(Ordering::SeqCst) && speak_retry < 3 {
                            speak_retry += 1;
                            handle.abort();
                            let force = format!(
                                "You are NOT allowed to end this turn yet: you have not spoken aloud to the user even once. \
This is a hard requirement — a turn may only end after exactly one SpeakToUser announcement. \
Call SpeakToUser ONCE right now with the following final response as the text (do not reply with text only, do not call any other tool):\n{}",
                                _final_text
                            );
                            let (rx2, handle2) = agent.query(&force).await;
                            rx = rx2;
                            handle = handle2;
                            continue;
                        }
                        // Result 是流结束标志：无论 text 是否为空都必须结束循环，
                        // 并确保前端收到 AssistantDone（文本用已收集的 _final_text 兜底）。
                        // 先持久化再广播 done：避免前端收到 done 后立即切换会话，
                        // 读到尚未落盘的 assistant 历史（切换后历史丢失的竞态根因）。
                        if !_final_text.is_empty() || !tool_log.is_empty() {
                            let mut sessions = core.sessions.lock().await;
                            sessions.append(&sid, "assistant", &_final_text, tool_log.clone());
                        }
                        persisted = true;
                        core.emit(Event::AssistantDone {
                            session_id: sid.clone(),
                            text: _final_text.clone(),
                        });
                        break;
                    }
                    Some(SDKMessage::Error { message }) => {
                        core.emit(Event::AssistantText {
                            session_id: sid.clone(),
                            text: format!("\n[错误] {message}"),
                        });
                    }
                    None => {
                        // 流异常关闭（非 Result 正常结束）也必须满足播报硬约束，
                        // 否则会出现"整轮无语音播报"的漏网路径。
                        if !core.turn_spoken.load(Ordering::SeqCst) && speak_retry < 3 {
                            speak_retry += 1;
                            let force = format!(
                                "You are NOT allowed to end this turn yet: you have not spoken aloud to the user even once. \
Call SpeakToUser ONCE right now with the following final response as the text (do not reply with text only, do not call any other tool):\n{}",
                                _final_text
                            );
                            let (rx2, handle2) = agent.query(&force).await;
                            rx = rx2;
                            handle = handle2;
                            continue;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = handle.await;

    if interrupted {
        agent.clear();
        core.emit(Event::AssistantDone {
            session_id: sid.clone(),
            text: "（语音指令已打断本轮）".into(),
        });
        // 被打断的一轮不持久化 assistant（避免把"已打断"占位写成真实历史）
    } else if !persisted {
        // 兜底：rx 异常关闭未走到 Result 分支时，已有内容仍落盘
        if !_final_text.is_empty() || !tool_log.is_empty() {
            let mut sessions = core.sessions.lock().await;
            sessions.append(&sid, "assistant", &_final_text, tool_log);
        }
    }

    core.main_busy.store(false, Ordering::SeqCst);
    core.set_main_status("idle");
}

fn extract_message_text(msg: &open_agent_sdk::Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

// ============================================================
// 子 Agent 任务执行（DispatchTask 后台调用）
// ============================================================

pub async fn run_subtask(core: Arc<CoreState>, task_id: String) {
    if let Err(e) = ensure_agents(&core).await {
        let mut tasks = core.tasks.lock().await;
        if let Some(t) = tasks.get_mut(&task_id) {
            t.status = "error".into();
            t.summary = e.clone();
        }
        core.emit(Event::SubagentError {
            task_id,
            message: e,
        });
        return;
    }

    let (name, instruction) = {
        let tasks = core.tasks.lock().await;
        match tasks.get(&task_id) {
            Some(t) => (t.name.clone(), t.instruction.clone()),
            None => return,
        }
    };

    core.sub_busy.store(true, Ordering::SeqCst);
    core.interrupt_sub.store(false, Ordering::SeqCst);
    core.set_sub_status("working");

    let mut guard = core.sub_agent.lock().await;
    let agent = match guard.as_mut() {
        Some(a) => a,
        None => {
            core.sub_busy.store(false, Ordering::SeqCst);
            core.set_sub_status("idle");
            return;
        }
    };

    let (mut rx, handle) = agent.query(&instruction).await;

    let mut final_text = String::new();
    let mut interrupted = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if core.interrupt_sub.load(Ordering::SeqCst) {
                    interrupted = true;
                    handle.abort();
                    break;
                }
            }
            ev = rx.recv() => {
                match ev {
                    Some(SDKMessage::PartialMessage { text }) => {
                        if final_text.starts_with(&text) {
                            final_text = text.clone();
                        } else {
                            final_text.push_str(&text);
                        }
                        core.emit(Event::SubagentProgress { task_id: task_id.clone(), message: text });
                    }
                    Some(SDKMessage::Assistant { message, .. }) => {
                        for block in &message.content {
                            if let ContentBlock::ToolUse { name, input, .. } = block {
                                core.emit(Event::SubagentToolUse {
                                    task_id: task_id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                });
                            } else if let ContentBlock::Thinking { thinking, .. } = block {
                                if !thinking.is_empty() {
                                    core.emit(Event::SubagentProgress {
                                        task_id: task_id.clone(),
                                        message: format!("思考：{}", thinking),
                                    });
                                }
                            }
                        }
                        let t = extract_message_text(&message);
                        if !t.is_empty() {
                            core.emit(Event::SubagentProgress { task_id: task_id.clone(), message: t });
                        }
                    }
                    Some(SDKMessage::ToolResult { tool_name, content, is_error, .. }) => {
                        let summary = content.chars().take(300).collect::<String>();
                        core.emit(Event::SubagentToolResult {
                            task_id: task_id.clone(),
                            name: tool_name,
                            ok: !is_error,
                            summary,
                        });
                    }
                    Some(SDKMessage::Result { text, .. }) => {
                        if !text.is_empty() {
                            final_text = text.clone();
                        }
                        break;
                    }
                    Some(SDKMessage::Error { message }) => {
                        final_text = format!("错误：{message}");
                    }
                    None => break,
                    _ => {}
                }
            }
        }
    }
    let _ = handle.await;

    if interrupted {
        agent.clear();
    }
    drop(guard);

    core.sub_busy.store(false, Ordering::SeqCst);
    core.set_sub_status("idle");

    let (status, summary): (&str, String) = if interrupted {
        ("interrupted", "已被主 Agent 打断".into())
    } else if final_text.trim().is_empty() || final_text.starts_with("错误") {
        let s = if final_text.is_empty() {
            "子 Agent 未返回结果".into()
        } else {
            final_text.clone()
        };
        ("error", s)
    } else {
        ("done", final_text.clone())
    };

    {
        let mut tasks = core.tasks.lock().await;
        if let Some(t) = tasks.get_mut(&task_id) {
            t.status = status.into();
            t.summary = summary.clone();
        }
    }

    match status {
        "done" => {
            core.emit(Event::SubagentDone {
                task_id: task_id.clone(),
                summary: summary.clone(),
            });
            // 自动调主 Agent 语音汇报，然后回到待命监听
            let report = format!(
                "子 Agent 已完成任务「{}」。结果摘要：{}。\
                 请用简洁中文语音向用户汇报结果，然后回到待命监听状态。",
                name, summary
            );
            core.emit(Event::SubagentReportReady {
                task_id: task_id.clone(),
                text: report.clone(),
            });
            // 汇报绑定到语音目标会话（若无则跟随 active）。
            // is_report=true：结果以独立事件（非用户消息）进入主 Agent，且不写入用户历史。
            let vsid = core.voice_session.lock().await.clone();
            let _ = run_main_turn(core, report, vsid, true).await;
        }
        "interrupted" => {
            core.emit(Event::SubagentError {
                task_id: task_id.clone(),
                message: summary,
            });
        }
        _ => {
            core.emit(Event::SubagentError {
                task_id: task_id.clone(),
                message: summary,
            });
        }
    }
}

// ============================================================
// 唤醒词处理（voice-serve 回调）
// 情况1：主 Agent 工作中 → 识别 stop 打断；识别其它指令 → 打断后处理
// 情况2：主 Agent 空闲 → 直接进入指令监听
// ============================================================

/// 语音唤醒互斥守卫：KWS 可能因窗口滑动/余音对同一句话重复回调，
/// 只允许第一个进入 handle_wakeword，其余直接丢弃（Drop 时释放）。
struct VoiceGuard(Arc<CoreState>);
impl VoiceGuard {
    fn acquire(core: Arc<CoreState>) -> Option<Self> {
        if core.voice_active.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Self(core))
        }
    }
}
impl Drop for VoiceGuard {
    fn drop(&mut self) {
        self.0.voice_active.store(false, Ordering::SeqCst);
    }
}

pub async fn handle_wakeword(core: Arc<CoreState>, keyword: String) {
    let Some(_guard) = VoiceGuard::acquire(core.clone()) else {
        tracing::warn!("语音唤醒互斥：上一个唤醒处理未结束，丢弃重复回调");
        return;
    };
    let cfg = core.config.lock().await;
    // KWS 返回的关键词是模型输出的原始文本（如 "COMPUTER"/"HEY COMPUTER"），
    // 而 cfg.wakeword 是配置值（如 "computer"）：必须大小写不敏感、允许前缀修饰（hey）后包含匹配，
    // 否则命中后此处直接 return，整条链路静默中断（无提示音、无录音、无反应）。
    if !cfg.voice_enabled
        || !keyword
            .to_lowercase()
            .contains(&cfg.wakeword.to_lowercase())
    {
        return;
    }
    let beep = cfg.beep_file.clone();
    let max_record = cfg.max_record_secs;
    drop(cfg);

    core.emit(Event::Voice {
        kind: "wakeword".into(),
        text: keyword.clone(),
    });

    // 语音对话绑定到前端当前会话（voice_session 由前端在开启监听/切换会话时同步）
    let voice_sid = core.voice_session.lock().await.clone();

    // 播放唤醒提示音
    let _ = core.voice.beep(Some(&beep)).await;

    let main_busy = core.main_busy.load(Ordering::SeqCst);
    let speaking = core.speaking.load(Ordering::SeqCst);

    if main_busy || speaking {
        // 情况1：打断当前 TTS
        let _ = core.voice.interrupt().await;
        let text = core.voice.listen_once(max_record).await;
        match text {
            Ok(t) => {
                let t = t.trim().to_string();
                core.emit(Event::Voice {
                    kind: "stt".into(),
                    text: t.clone(),
                });
                let cfg = core.config.lock().await;
                let is_stop = cfg
                    .interrupt_keywords
                    .iter()
                    .any(|k| t.to_lowercase().contains(&k.to_lowercase()));
                drop(cfg);
                if is_stop {
                    // 仅打断，不处理新指令
                    core.interrupt_main.store(true, Ordering::SeqCst);
                    core.emit(Event::Voice {
                        kind: "stt".into(),
                        text: "（识别到打断指令）".into(),
                    });
                } else {
                    core.interrupt_main.store(true, Ordering::SeqCst);
                    // 等待当前轮被 abort
                    for _ in 0..200 {
                        if !core.main_busy.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let _ = run_main_turn(core, t, voice_sid.clone(), false).await;
                }
            }
            Err(e) => {
                core.emit(Event::Voice {
                    kind: "stt".into(),
                    text: format!("识别失败：{e}"),
                });
            }
        }
    } else {
        // 情况2：空闲，直接监听指令
        let text = core.voice.listen_once(max_record).await;
        match text {
            Ok(t) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    return;
                }
                core.emit(Event::Voice {
                    kind: "stt".into(),
                    text: t.clone(),
                });
                let _ = run_main_turn(core, t, voice_sid.clone(), false).await;
            }
            Err(e) => {
                core.emit(Event::Voice {
                    kind: "stt".into(),
                    text: format!("识别失败：{e}"),
                });
            }
        }
    }
}
