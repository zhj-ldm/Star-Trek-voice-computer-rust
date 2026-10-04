//! Agent 编排：构建主/子 Agent、主 Agent 会话轮、子 Agent 任务执行、
//! 唤醒词处理与自动语音汇报。主 Agent 只开放搜索+派发+监控+语音工具。

use crate::events::Event;
use crate::sessions::ToolCallRecord;
use crate::search::WebSearchTool;
use crate::state::CoreState;
use crate::tools::{auto_announce, ImportSkill};
use open_agent_sdk::tools::askuser::{AskUserFn, AskUserTool};
use open_agent_sdk::utils::messages::{create_assistant_message, create_user_message};
use open_agent_sdk::{Agent, AgentOptions, ApiClient, ContentBlock, Message, SDKMessage};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub type MainAgent = Agent;
pub type SubAgent = Agent;

// ============================================================
// 构建 Agent
// ============================================================

fn home_dir() -> String {
    crate::paths::home_dir().to_string_lossy().into_owned()
}

/// 本地 Ollama 模型名解析：
/// - 思考关闭（默认）且本地上游存在 `{model}-nothink` 变体时，自动改用带 `-nothink` 后缀的模型
///   （Modelfile 已内置关闭思考，响应更快、不输出 reasoning 内容）；
/// - 思考开启时，若模型名本身带 `-nothink` 后缀，则去掉后缀换回完整版模型。
/// SDK 层仍会携带 think:false/true 参数作为第二道保险（仅对本地上游）。
async fn resolve_model_name(
    model: &str,
    base_url: &str,
    enable_thinking: bool,
    cache: &Arc<tokio::sync::Mutex<Vec<String>>>,
) -> String {
    let is_local = base_url.contains("127.0.0.1") || base_url.contains("localhost");
    if !is_local || model.is_empty() {
        return model.to_string();
    }

    let has_nothink = model.ends_with("-nothink");
    if enable_thinking {
        // 开启思考：去掉 -nothink 后缀
        return if has_nothink {
            model.trim_end_matches("-nothink").to_string()
        } else {
            model.to_string()
        };
    }

    // 关闭思考（默认）：优先用 -nothink 变体（若本地存在）
    if has_nothink {
        return model.to_string();
    }
    let candidate = format!("{model}-nothink");

    // 用缓存的本地模型列表判断变体是否存在；缓存为空时刷新一次
    let mut list = cache.lock().await;
    if list.is_empty() {
        if let Ok(names) = fetch_ollama_models(base_url).await {
            *list = names;
        }
    }
    if list.iter().any(|n| n == &candidate) {
        tracing::info!("思考已关闭：本地存在 {candidate}，自动使用该模型");
        return candidate;
    }
    model.to_string()
}

/// 拉取本地 Ollama 模型列表（/api/tags）。
async fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let host = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1");
    let url = format!("{host}/api/tags");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let mut names = Vec::new();
    if let Some(models) = body.get("models").and_then(|v| v.as_array()) {
        for m in models {
            if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
                names.push(name.to_string());
            }
        }
    }
    Ok(names)
}

/// 构建唯一 Agent（单 Agent 架构）：具备全部工具能力
/// （SDK 默认 bash/文件/web/tasks + 语音播报 + 导入 Skill + 真实 WebSearch），
/// 请求按 round-robin 分摊到配置的多个上游（多 API key），每个上游独立遵守自己的 RPM 限速。
async fn build_main_agent(core: Arc<CoreState>) -> Result<MainAgent, String> {
    let cfg = core.config.lock().await;
    let skills_text = core.skills.usage_text().await;
    let memory_text = core.memory.context_text(30).await;
    // 真正的系统提示词（核心行为规则）以 resources/prompts/system.md 文件为准：
    // 文件缺失时回退内置精简版；人设（main_system_prompt）由用户在设置中编辑，追加在规则之后。
    let mut system_rules = load_system_rules();
    let ask_user_on = cfg.ask_user_enabled;
    if !ask_user_on {
        // 关闭中途询问时，只移除「主动询问」这一节（到下一个 "## " 标题为止），
        // 避免模型调用未注册工具；其余规则（语音播报/风格/技能）必须保留。
        if let Some(idx) = system_rules.find("## 3. 主动询问用户") {
            let rest = &system_rules[idx..];
            let end = rest
                .find("\n## ")
                .map(|i| idx + i + 1)
                .unwrap_or(system_rules.len());
            system_rules = format!("{}{}", &system_rules[..idx], &system_rules[end..]);
            system_rules = system_rules.trim_end().to_string();
        }
    }
    let append = format!(
        "\n\n=== 可用 Skills ===\n{}\n\n=== 长期记忆 ===\n{}",
        skills_text, memory_text
    );
    let mut opts = AgentOptions::default();
    // 多上游分摊：providers 是"同服务商同模型"的多个请求通道（多 Key / 多入口），
    // 模型统一使用全局 main_model——轮询只切通道、绝不切模型。
    // 每个通道持有独立 ApiClient（各自 RPM 限流）；为空时退回单端点 main_base_url。
    if !cfg.providers.is_empty() {
        let mut clients = Vec::new();
        // 当前使用的模型配置：active_model 匹配 name；为空/找不到 → 第一个。
        // 每套配置 = 一个模型（URL + 模型名 + 多 API key 轮询分摊 RPM）。
        let active = cfg
            .providers
            .iter()
            .find(|p| !cfg.active_model.is_empty() && p.name == cfg.active_model)
            .or_else(|| cfg.providers.first())
            .cloned();
        if let Some(ap) = active {
            if ap.base_url.trim().is_empty() {
                return Err(format!("模型配置「{}」缺少 Base URL，请检查设置", ap.name));
            }
            // 多个 API Key → 每个 Key 展开为独立通道（同 URL、同模型）轮询分摊；
            // 否则退回单 Key（api_key / 空 Key 本地模型）。
            let keys: Vec<String> = if !ap.api_keys.is_empty() {
                ap.api_keys.clone()
            } else {
                vec![ap.api_key.clone()]
            };
            let model = resolve_model_name(
                if ap.model.trim().is_empty() { &cfg.main_model } else { &ap.model },
                &ap.base_url,
                cfg.enable_thinking,
                &core.ollama_models_cache,
            ).await;
            for key in keys {
                let mut c = ApiClient::new(
                    Some(key),
                    Some(ap.base_url.clone()),
                    Some(model.clone()),
                );
                c.set_rpm_limit(ap.rpm_limit.unwrap_or(cfg.rpm_limit));
                c.set_enable_thinking(cfg.enable_thinking);
                clients.push(c);
            }
        }
        if clients.is_empty() {
            return Err("providers 列表为空或全部 base_url 为空，请在设置中配置至少一个 API".into());
        }
        opts.api_clients = clients;
    } else {
        opts.model = Some(resolve_model_name(
            &cfg.main_model,
            &cfg.main_base_url,
            cfg.enable_thinking,
            &core.ollama_models_cache,
        ).await);
        opts.api_key = Some(cfg.main_api_key.clone());
        opts.base_url = Some(cfg.main_base_url.clone());
        opts.rpm_limit = Some(cfg.rpm_limit);
        opts.enable_thinking = Some(cfg.enable_thinking);
    }
    opts.cwd = Some(home_dir());
    opts.system_prompt = Some(format!("{}\n\n{}", system_rules, cfg.main_system_prompt));
    opts.append_system_prompt = Some(append);
    opts.max_turns = Some(cfg.max_turns.min(100));
    // 全部功能：导入 Skill + 真实 WebSearch（覆盖 SDK 占位版）+ 中途语音询问；
    // 其余 bash/文件/WebFetch/AskUser/Tasks 等由 SDK 默认注册，全部开放。
    // 语音播报不再注册为 AI 工具：每轮结束后由 core 自动用最终文本 TTS 播报。
    let mut custom_tools: Vec<Arc<dyn open_agent_sdk::Tool>> = vec![
        Arc::new(ImportSkill::new(core.clone())),
        Arc::new(WebSearchTool::default()),
    ];
    // 中途询问用户：AI 可主动播报问题并聆听用户的语音回复（设置中可关闭）。
    if ask_user_on {
        let ask_core = core.clone();
        let ask_fn: AskUserFn = Arc::new(move |question: &str| {
            let core = ask_core.clone();
            let q = question.to_string();
            Box::pin(async move {
                // 1) 用当前音色/语速播报问题
                let (voice, rate) = {
                    let cfg = core.config.lock().await;
                    (cfg.voice.clone(), cfg.rate)
                };
                let _ = core.voice.speak(&q, &voice, rate).await;
                // 2) 播「开始录音」提示音（短哔，提示用户现在可以回答了），播完再静音
                {
                    let beep_path = crate::paths::resource("computer_beep_1.mp3")
                        .to_string_lossy()
                        .into_owned();
                    let _ = core.voice.beep(Some(&beep_path)).await;
                }
                // 3) 静音系统输出（避免 TTS 回声/其他声音干扰识别），识别后恢复
                system_mute(true);
                let result = core.voice.listen_once(15.0).await;
                system_mute(false);
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
                    Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
                    Ok(_) => Err("未能听清用户的语音回复".to_string()),
                    Err(e) => Err(format!("语音识别失败: {e}")),
                }
            })
        });
        custom_tools.push(Arc::new(AskUserTool::new(ask_fn)));
    }
    opts.custom_tools = custom_tools;
    // 单 Agent：显式禁用 SDK 默认注册的任务派发/团队/消息/计划/工作区类工具，
    // 只保留对用户有用的能力（搜索/网页/文件/bash/定时任务/语音/记忆等），
    // 从根源上杜绝模型"派发子任务"。
    opts.disallowed_tools = Some(vec![
        "TaskCreate".into(),
        "TaskGet".into(),
        "TaskList".into(),
        "TaskUpdate".into(),
        "TaskStop".into(),
        "TaskOutput".into(),
        "SendMessage".into(),
        "TeamCreate".into(),
        "TeamDelete".into(),
        "EnterPlanMode".into(),
        "ExitPlanMode".into(),
        "EnterWorktree".into(),
        "ExitWorktree".into(),
    ]);
    Agent::new(opts).await
}

/// 确保唯一 Agent 已构建（API 配置就绪）
pub async fn ensure_agents(core: &Arc<CoreState>) -> Result<(), String> {
    if core.agents_ready.load(Ordering::SeqCst) {
        return Ok(());
    }
    {
        let cfg = core.config.lock().await;
        let has_provider = !cfg.providers.is_empty()
            && cfg.providers.iter().any(|p| !p.base_url.trim().is_empty());
        let has_legacy = !cfg.main_base_url.trim().is_empty();
        if !has_provider && !has_legacy {
            return Err("请先在设置中配置至少一个 API（地址/Key/模型）".into());
        }
    }
    {
        let mut mg = core.main_agent.lock().await;
        if mg.is_none() {
            *mg = Some(build_main_agent(core.clone()).await?);
        }
    }
    core.agents_ready.store(true, Ordering::SeqCst);
    core.set_main_status("idle");
    core.set_sub_status("idle");
    Ok(())
}

/// 重建唯一 Agent（设置变更/记忆变更后调用）
pub async fn rebuild_agents(core: &Arc<CoreState>) {
    {
        let mut mg = core.main_agent.lock().await;
        *mg = None;
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
    *core.turn_session.lock().await = Some(sid.clone());
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
            *core.turn_session.lock().await = None;
            core.set_main_status("idle");
            return;
        }
    };

    // 恢复当前会话历史到 Agent 上下文（切换会话后上下文相互独立）。
    // 注意：此时尚未 append 当前 user，恢复的历史不含当前输入，
    // 避免 query() 内部再 push 一次 user 导致消息双发。
    // 上下文压缩：消息超过 COMPACT_KEEP 条时，早期部分折叠为一条摘要，
    // 仅保留最近 COMPACT_KEEP 条明文，控制历史膨胀与 token 消耗。
    {
        const COMPACT_KEEP: usize = 20;
        let history = {
            let sessions = core.sessions.lock().await;
            let msgs = sessions.recent_messages(&sid, 40);
            let mut out: Vec<Message> = Vec::new();
            if msgs.len() > COMPACT_KEEP {
                let old = &msgs[..msgs.len() - COMPACT_KEEP];
                let mut buf =
                    String::from("[早期对话已压缩为摘要，仅作背景参考，无需回应]\n");
                for m in old {
                    let role = if m.role == "user" { "用户" } else { "助手" };
                    let text: String = m.text.chars().take(120).collect();
                    buf.push_str(&format!("{role}: {text}\n"));
                }
                out.push(create_user_message(&buf));
                for m in &msgs[msgs.len() - COMPACT_KEEP..] {
                    if m.role == "user" {
                        out.push(create_user_message(&m.text));
                    } else {
                        out.push(create_assistant_message(&m.text));
                    }
                }
            } else {
                for m in &msgs {
                    if m.role == "user" {
                        out.push(create_user_message(&m.text));
                    } else {
                        out.push(create_assistant_message(&m.text));
                    }
                }
            }
            out
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
        let complete_file = crate::paths::resource("complete.mp3");
        let _ = core.voice.beep(Some(&complete_file.to_string_lossy())).await;
    }

    let (mut rx, handle) = agent.query(&user_text).await;

    let mut _final_text = String::new();
    let mut interrupted = false;
    let mut persisted = false; // assistant 是否已在 Result 分支落盘
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
                                    done: false,
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
                        tracing::info!("工具结果事件: {tool_name} ok={}", !is_error);
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
                            rec.done = true;
                        }
                    }
                    Some(SDKMessage::Result { text, .. }) => {
                        if !text.is_empty() {
                            _final_text = text.clone();
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
                        // 流异常关闭（非 Result 正常结束）：直接结束循环，
                        // 已有内容由循环后兜底落盘 + 自动播报处理。
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
        // 补发中断状态：还在"运行中"的工具卡片立即收尾为"已中断"
        for rec in tool_log.iter().filter(|r| !r.done) {
            core.emit(Event::ToolResult {
                session_id: sid.clone(),
                agent: "main".into(),
                name: rec.name.clone(),
                ok: false,
                summary: "已中断".into(),
            });
        }
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
    *core.turn_session.lock().await = None;
    core.set_main_status("idle");

    // 自动语音播报：轮结束后由 core 直接播报最终文本，不依赖 AI 调用语音工具。
    // 被打断的一轮不播报；TTS 在 auto_announce 内部异步执行，不阻塞返回。
    if !interrupted {
        auto_announce(core.clone(), _final_text.clone()).await;
    }
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

/// 系统级静音/恢复：唤醒音播放完毕后立刻压掉系统所有声音（含正在播放的
/// 音乐/视频），录音结束后恢复。用 osascript 控制 macOS 输出，mute/unmute
/// 不改变音量值，恢复时回到原音量。
fn system_mute(muted: bool) {
    let script = if muted {
        "set volume with output muted"
    } else {
        "set volume without output muted"
    };
    let _ = std::process::Command::new("osascript").arg("-e").arg(script).output();
}

/// 静音状态下录制一轮用户语音：进入即静音，录音结束（无论成功失败）即恢复。
async fn listen_once_muted(
    core: Arc<CoreState>,
    max_secs: f64,
) -> Result<String, anyhow::Error> {
    system_mute(true);
    let r = core.voice.listen_once(max_secs).await;
    system_mute(false);
    r
}

pub async fn handle_wakeword(core: Arc<CoreState>, keyword: String) {
    // === 阶段1：唤醒互斥 + 提示音 + 录音（guard 只在此阶段持有） ===
    // AI 处理阶段（run_main_turn）不持有 guard：否则 AI 处理几分钟期间，
    // 用户再喊 computer 全部被互斥丢弃（无提示音、无反应）——"唤不醒"根因。
    // 释放后 AI 处理中的新 computer 命中会走 busy 打断分支，提示音正常。
    let stage = {
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

        let main_busy = core.main_busy.load(Ordering::SeqCst);
        let speaking = core.speaking.load(Ordering::SeqCst);

        if main_busy || speaking {
            // AI 工作/播报中喊 computer：只播打断音，不播唤醒音（否则两声混乱）。
            // 不录音接话；AI 停下回到监听后可再喊 computer 接新指令。
            let _ = core.voice.interrupt().await;
            core.interrupt_main.store(true, Ordering::SeqCst);
            let beep_path = crate::paths::resource("computerbeep_75.mp3")
                .to_string_lossy()
                .into_owned();
            let v = core.voice.clone();
            tokio::spawn(async move {
                let _ = v.beep(Some(&beep_path)).await;
            });
            core.emit(Event::Voice {
                kind: "stt".into(),
                text: "（语音打断：computer）".into(),
            });
            return;
        }

        // 空闲：先播唤醒提示音（等播完，避免被录音静音吞掉）再录音
        let voice_sid = core.voice_session.lock().await.clone();
        let _ = core.voice.beep(Some(&beep)).await;
        let text = listen_once_muted(core.clone(), max_record).await;
        (voice_sid, text)
    };

    // === 阶段2：guard 已释放，跑 AI（期间新 computer 命中可正常打断） ===
    let (voice_sid, text) = stage;
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
            let _ = run_main_turn(core, t, voice_sid, false).await;
        }
        Err(e) => {
            core.emit(Event::Voice {
                kind: "stt".into(),
                text: format!("识别失败：{e}"),
            });
        }
    }
}


/// 读取系统提示词规则文件（resources/prompts/system.md）；文件缺失时回退内置精简版。
/// 系统规则为强制约束，独立于用户人设（设置中的 System Prompt）。
fn load_system_rules() -> String {
    let path = crate::paths::project_root().join("resources/prompts/system.md");
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => "\
# 核心行为规则
- 你是用户唯一的 AI 助手，所有任务由你自己用工具直接完成。
- 依赖外部信息时必须先调用工具拿到真实结果，禁止编造工具、搜索结果、文件内容、命令输出和链接。
- 回复末尾必须单独一行以【Voice】开头写中文播报，不超过100字，口语化，无任何符号，必须完整写出要朗读的句子。
- 用中文回答，简洁、快速、专业，像精炼的星舰通讯，正文1~3句。
- 优先使用可用 Skills 完成任务；参考长期记忆中的用户偏好。"
            .to_string(),
    }
}
