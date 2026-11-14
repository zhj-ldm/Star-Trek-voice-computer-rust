//! Agent 编排：构建主 Agent、主 Agent 会话轮、
//! 唤醒词处理与自动语音汇报。主 Agent 只开放搜索+派发+监控+语音工具。

use crate::config::Config;
use crate::events::Event;
use crate::search::WebSearchTool;
use crate::sessions::ToolCallRecord;
use crate::state::CoreState;
use crate::tools::{auto_announce, AskUserTool, DeliverFiles, ToolGuide, ToolHub};
use rpi_agent::AgentTool;
use rpi_ai::types::{
    Api, AssistantMessage, AssistantMessageEvent, AssistantRole, Content, Context, Message,
    StopReason, ThinkingLevel, ToolCall, ToolResultMessage, ToolResultRole, Usage,
    UserMessage,
};
use rpi_ai::{Model, Provider, SimpleStreamOptions};
use rpi_ai::model::{
    MaxTokensField, OpenaiCompletionsCompat, OpenaiThinkingFormat, StreamingProtocolCompat,
};
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_tools::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, ExecutionToolContext,
    OsExecutionEnv,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 主 Agent 运行时：按轮惰性构建请求，不持有跨轮上下文（历史由 SessionStore 提供）。
pub struct MainAgent {
    channels: Vec<Channel>,
    /// round-robin 游标：多 Key 轮询分摊 RPM
    cursor: AtomicUsize,
    model: Model,
    /// 思考强度（None = 关闭）
    reasoning: Option<ThinkingLevel>,
    system_prompt: String,
    max_turns: usize,
    hub: Arc<ToolHub>,
    ask_user: bool,
    /// true = 工具全部默认加载；false = 渐进式（需要时经 ToolGuide 加载）
    eager_tools: bool,
}

struct Channel {
    provider: OpenAiCompletionsProvider,
    api_key: Option<String>,
    rpm: u32,
    /// 上次请求时间（用于客户端侧 RPM 限速）
    last: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl Channel {
    /// 等待到自己可用的时间片（简单均匀限速：间隔 60/rpm 秒）。
    async fn wait_slot(&self) {
        if self.rpm == 0 {
            return;
        }
        let interval = Duration::from_secs_f64(60.0 / self.rpm as f64);
        let mut last = self.last.lock().await;
        if let Some(t) = *last {
            let elapsed = t.elapsed();
            if elapsed < interval {
                tokio::time::sleep(interval - elapsed).await;
            }
        }
        *last = Some(std::time::Instant::now());
    }
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage::new(text.to_string(), 0))
}

fn assistant_msg(text: &str, model: &str) -> Message {
    Message::Assistant(Box::new(AssistantMessage {
        role: AssistantRole,
        content: vec![Content::text(text)],
        api: Api::OpenaiCompletions,
        provider: "custom".into(),
        model: model.to_string(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }))
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

/// 构建本轮可用工具：常驻工具 + （渐进式时）已加载的懒加载工具。
/// 渐进式额外提供唯一入口 ToolGuide，其说明里列出全量索引。
async fn build_turn_tools(
    hub: &Arc<ToolHub>,
    sid: &str,
    ask_user: bool,
    eager: bool,
) -> Vec<Arc<dyn AgentTool>> {
    let env = Arc::new(OsExecutionEnv::with_cwd(crate::paths::home_dir()));
    let ctx = ExecutionToolContext::new(
        env.clone(),
        Some(env.clone() as Arc<dyn rpi_tools::MutatingEnv>),
    );
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        create_read_tool(&ctx, None),
        create_write_tool(&ctx),
        create_edit_tool(&ctx),
        create_bash_tool(&ctx, None),
        Arc::new(WebSearchTool::new()),
        Arc::new(DeliverFiles::new()),
    ];
    if ask_user {
        tools.push(Arc::new(AskUserTool::new(hub.core().clone())));
    }
    if eager {
        tools.extend(hub.all_tools());
    } else {
        // 已按需加载的工具 + 用户在设置里勾选「默认加载」的工具。
        // 用户可为每个懒加载工具单独开关，不是一律全加也不是固定预设。
        let defaults = hub
            .core()
            .config
            .lock()
            .await
            .default_loaded_tools
            .clone();
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in hub.loaded_tools(sid).await {
            names.insert(t.schema().name.clone());
            tools.push(t);
        }
        for t in hub.all_tools() {
            let name = t.schema().name.clone();
            if defaults.iter().any(|n| n == &name) && names.insert(name) {
                tools.push(t);
            }
        }
        tools.push(ToolGuide::new(hub.clone(), sid.to_string()).await);
    }
    tools
}

// ============================================================
// 构建 Agent
// ============================================================

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

/// 按思考强度挑选 DeepSeek 模型：思考开启时把 deepseek-chat 升级为 deepseek-reasoner；
/// 用户显式选择 reasoner / flash 或其它型号时保持原模型。
fn apply_thinking_model(model: &str, base_url: &str, thinking_on: bool) -> String {
    if base_url.contains("deepseek.com") {
        match (model, thinking_on) {
            ("deepseek-chat", true) => return "deepseek-reasoner".into(),
            // 用户显式选了 reasoner / flash 时尊重其选择，不强行改写
            _ => {}
        }
    }
    model.to_string()
}

/// 拉取本地 Ollama 模型列表（/api/tags）。
async fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>, String> {
    let host = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let url = format!("{host}/api/tags");
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
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

/// 构建主 Agent 运行时：把配置解析成一个模型 + 多条 Key 通道（round-robin / 独立 RPM）。
async fn build_main_agent(core: Arc<CoreState>) -> Result<MainAgent, String> {
    let cfg = core.config.lock().await;
    let skills_text = core.skills.usage_text().await;
    let memory_text = core.memory.context_text(30).await;
    let thinking_on = cfg.thinking_level != "off";

    // 解析当前生效的上游：providers 非空时取 active_model 指向的配置，否则用旧字段。
    let (base_url, raw_model, rpm, keys) = if !cfg.providers.is_empty() {
        let active = cfg
            .providers
            .iter()
            .find(|p| !cfg.active_model.is_empty() && p.name == cfg.active_model)
            .or_else(|| cfg.providers.first())
            .cloned()
            .ok_or_else(|| "providers 列表为空，请在设置中配置至少一个 API".to_string())?;
        if active.base_url.trim().is_empty() {
            return Err(format!("模型配置「{}」缺少 Base URL，请检查设置", active.name));
        }
        let keys: Vec<String> = if !active.api_keys.is_empty() {
            active.api_keys.clone()
        } else {
            vec![active.api_key.clone()]
        };
        let model = if active.model.trim().is_empty() {
            cfg.main_model.clone()
        } else {
            active.model.clone()
        };
        (
            active.base_url.clone(),
            model,
            active.rpm_limit.unwrap_or(cfg.rpm_limit),
            keys,
        )
    } else {
        (
            cfg.main_base_url.clone(),
            cfg.main_model.clone(),
            cfg.rpm_limit,
            vec![cfg.main_api_key.clone()],
        )
    };

    let model_name = resolve_model_name(
        &raw_model,
        &base_url,
        thinking_on,
        &core.ollama_models_cache,
    )
    .await;
    let model_name = apply_thinking_model(&model_name, &base_url, thinking_on);

    let mut model = Model::new(
        model_name.clone(),
        model_name.clone(),
        Api::OpenaiCompletions,
        "custom",
        base_url.trim_end_matches('/').to_string(),
    );
    // 本地/云端是否真的思考由模型决定；这里只影响是否携带思考参数。
    // DeepSeek 例外：无论思考强度如何都携带 thinking=enabled，
    // 否则其默认行为可能不返回 reasoning_content，UI 就拿不到思考过程。
    let deepseek = base_url.contains("deepseek.com");
    model.reasoning = thinking_on || deepseek;
    // 兼容层：绝大多数 OpenAI 兼容端点只认 max_tokens；DeepSeek 用 thinking 字段开启思考。
    let mut compat = OpenaiCompletionsCompat::default();
    if !base_url.contains("api.openai.com") {
        compat.max_tokens_field = Some(MaxTokensField::MaxTokens);
    }
    if base_url.contains("deepseek.com") {
        compat.supports_reasoning_effort = Some(false);
        compat.thinking_format = Some(OpenaiThinkingFormat::Deepseek);
        compat.requires_reasoning_content = Some(true);
        compat.reasoning_counts_toward_max_tokens = Some(true);
    }
    model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(compat));
    // 显式给正文足够的输出额度。默认 0 表示不发送 max_tokens，
    // 部分中转端会按很小的默认值截断，长思考或写大文件时会出现
    // 「思考完、工具调用被截断、正文为空」的假完成。
    model.max_tokens = 8192;

    let mut channels = Vec::new();
    for key in keys {
        let api_key = if key.trim().is_empty() {
            None
        } else {
            Some(key.trim().to_string())
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| e.to_string())?;
        let provider = match api_key.clone() {
            Some(_) => OpenAiCompletionsProvider::with_models_without_env_api_key(
                "custom",
                api_key.clone(),
                http,
                vec![model.clone()],
            ),
            None => OpenAiCompletionsProvider::with_models_without_auth(
                "custom",
                http,
                vec![model.clone()],
            ),
        };
        channels.push(Channel {
            provider,
            api_key,
            rpm,
            last: tokio::sync::Mutex::new(None),
        });
    }
    if channels.is_empty() {
        return Err("没有可用的 API 通道，请检查设置中的地址/Key/模型".into());
    }

    let hub = ToolHub::new(core.clone());
    let reasoning = match cfg.thinking_level.as_str() {
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        // DeepSeek 强度关闭时仍按最低档请求，保证 reasoning_content 持续返回
        _ if deepseek => Some(ThinkingLevel::Low),
        _ => None,
    };

    Ok(MainAgent {
        channels,
        cursor: AtomicUsize::new(0),
        model,
        reasoning,
        system_prompt: compose_system_prompt(&cfg, &skills_text, &memory_text),
        max_turns: cfg.max_turns.clamp(1, 100) as usize,
        hub,
        ask_user: cfg.ask_user_enabled,
        eager_tools: cfg.tool_loading == "eager",
    })
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
            elapsed_ms: 0,
            interrupted: false,
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
    // 本轮墙钟计时起点：用于"已完成，用时 X 分 X 秒"（含思考与工具全过程）
    let t0 = std::time::Instant::now();
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

    // 恢复当前会话历史到 Agent 上下文（切换会话后上下文相互独立）。
    // 注意：此时尚未 append 当前 user，恢复的历史不含当前输入，
    // 避免 query() 内部再 push 一次 user 导致消息双发。
    // 上下文压缩：消息超过 COMPACT_KEEP 条时，早期部分折叠为一条摘要，
    // 仅保留最近 COMPACT_KEEP 条明文，控制历史膨胀与 token 消耗。
    // 先取历史快照（不含本轮输入），随后把当前输入追加进消息列表。
    let history = {
        const COMPACT_KEEP: usize = 20;
        let sessions = core.sessions.lock().await;
        let msgs = sessions.recent_messages(&sid, 40);
        let mut out: Vec<Message> = Vec::new();
        if msgs.len() > COMPACT_KEEP {
            let old = &msgs[..msgs.len() - COMPACT_KEEP];
            let mut buf = String::from("[早期对话已压缩为摘要，仅作背景参考，无需回应]\n");
            for m in old {
                let role = if m.role == "user" { "用户" } else { "助手" };
                let text: String = m.text.chars().take(120).collect();
                buf.push_str(&format!("{role}: {text}\n"));
            }
            out.push(user_msg(&buf));
            for m in &msgs[msgs.len() - COMPACT_KEEP..] {
                if m.role == "user" {
                    out.push(user_msg(&m.text));
                } else if !m.interrupted && !m.text.trim().is_empty() {
                    // 空白助手消息（如工具轮次无最终正文）没有信息量，
                    // 发给严格端点还会触发 400，直接跳过。
                    // 被打断的占位也不进入上下文，避免模型把"被打断"当成回答。
                    out.push(assistant_msg(&m.text, "history"));
                }
            }
        } else {
            for m in &msgs {
                if m.role == "user" {
                    out.push(user_msg(&m.text));
                } else if !m.interrupted && !m.text.trim().is_empty() {
                    out.push(assistant_msg(&m.text, "history"));
                }
            }
        }
        out
    };

    // 当前 user 消息持久化（历史恢复之后再写，保证恢复的历史不含当前输入）。
    // 子 Agent 汇报不入用户历史，避免污染会话（避免切换会话后把汇报当作用户指令重放）。
    if !is_report {
        let mut sessions = core.sessions.lock().await;
        sessions.append(&sid, "user", &user_text, Vec::new(), 0);
    }

    // 播放“已发送”提示音（原版语义：把用户输入发给 AI 时播放 complete.mp3）。
    // 子 Agent 汇报属自动流程，不播放该提示音。
    // 连续对话模式：改用项目内固定的多轮对话音效（resources/computerbeep_29.mp3）。
    if !is_report {
        let file = if core.continuous_active.load(Ordering::SeqCst) {
            continuous_effect_path()
        } else {
            None
        }
        .unwrap_or_else(|| {
            crate::paths::resource("complete.mp3")
                .to_string_lossy()
                .into_owned()
        });
        let _ = core.voice.beep(Some(&file)).await;
    }

    // 进入内置 Agent 循环（唯一后端）。
    let guard = core.main_agent.lock().await;
    let agent = match guard.as_ref() {
        Some(a) => a,
        None => {
            core.main_busy.store(false, Ordering::SeqCst);
            *core.turn_session.lock().await = None;
            core.set_main_status("idle");
            return;
        }
    };

    // 历史恢复不含当前输入（上面先取快照、再持久化 user），此处追加当前 user。
    let mut messages = history;
    messages.push(user_msg(&user_text));

    let cancel = CancellationToken::new();
    let mut final_text = String::new();
    let mut interrupted = false;
    let mut reached_max = false;
    let mut tool_log: Vec<ToolCallRecord> = Vec::new();
    let max_turns = agent.max_turns;
    // 上游截断或只思考不产出时的续跑次数，避免静默结束。
    let mut stall_retries = 0u32;

    // 自管循环：请求（常驻+已加载工具）→ 流式解析 → 执行工具 → 回填 → 继续。
    // 渐进式加载的关键：每轮重新收集工具集，ToolGuide 启用的工具下一轮即可见。
    for turn in 0..max_turns {
        if core.interrupt_main.load(Ordering::SeqCst) {
            interrupted = true;
            cancel.cancel();
            break;
        }
        let tools = build_turn_tools(&agent.hub, &sid, agent.ask_user, agent.eager_tools).await;
        let ctx = Context {
            system_prompt: Some(agent.system_prompt.clone()),
            messages: messages.clone(),
            tools: tools.iter().map(|t| t.schema().clone()).collect(),
        };
        let ch = &agent.channels[agent.cursor.fetch_add(1, Ordering::SeqCst) % agent.channels.len()];
        ch.wait_slot().await;
        let opts = SimpleStreamOptions {
            api_key: ch.api_key.clone(),
            max_retries: Some(3),
            signal: cancel.clone(),
            reasoning: agent.reasoning,
            ..Default::default()
        };
        let mut stream = ch.provider.stream_simple(&agent.model, &ctx, &opts).await;

        let mut round_text = String::new();
        let mut round_thinking = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut assistant: Option<AssistantMessage> = None;
        let mut failure: Option<String> = None;

        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(30)) => {
                    if core.interrupt_main.load(Ordering::SeqCst) {
                        interrupted = true;
                        cancel.cancel();
                        break;
                    }
                }
                ev = stream.next() => {
                    match ev {
                        Some(AssistantMessageEvent::TextDelta { delta, .. }) => {
                            if !delta.is_empty() {
                                round_text.push_str(&delta);
                                core.emit(Event::AssistantText {
                                    session_id: sid.clone(),
                                    text: delta,
                                });
                            }
                        }
                        Some(AssistantMessageEvent::ThinkingDelta { delta, .. }) => {
                            if !delta.is_empty() {
                                round_thinking.push_str(&delta);
                                core.emit(Event::ReasoningText {
                                    session_id: sid.clone(),
                                    text: delta,
                                });
                            }
                        }
                        Some(AssistantMessageEvent::ToolCallEnd { tool_call, .. }) => {
                            core.emit(Event::ToolUse {
                                session_id: sid.clone(),
                                agent: "main".into(),
                                name: tool_call.name.clone(),
                                input: tool_call.arguments.clone(),
                            });
                            tool_log.push(ToolCallRecord {
                                name: tool_call.name.clone(),
                                input: tool_call.arguments.clone(),
                                ok: true,
                                summary: String::new(),
                                done: false,
                            });
                            calls.push(tool_call);
                        }
                        Some(AssistantMessageEvent::Done { message, .. }) => {
                            // 兜底：部分端点不逐段推 thinking/text，只在最终消息里带内容。
                            if round_thinking.trim().is_empty() {
                                let t = assistant_thinking_text(&message);
                                if !t.is_empty() {
                                    round_thinking = t.clone();
                                    core.emit(Event::ReasoningText {
                                        session_id: sid.clone(),
                                        text: t,
                                    });
                                }
                            }
                            if round_text.trim().is_empty() {
                                let t = assistant_text_of(&message);
                                if !t.is_empty() {
                                    round_text = t.clone();
                                    core.emit(Event::AssistantText {
                                        session_id: sid.clone(),
                                        text: t,
                                    });
                                }
                            }
                            assistant = Some(message);
                            break;
                        }
                        Some(AssistantMessageEvent::Error { error, .. }) => {
                            if error.stop_reason == StopReason::Aborted || interrupted {
                                interrupted = true;
                                cancel.cancel();
                            } else {
                                failure = Some(
                                    error
                                        .error_message
                                        .clone()
                                        .unwrap_or_else(|| "模型请求失败".into()),
                                );
                            }
                            break;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
        }

        let stop_reason = assistant
            .as_ref()
            .map(|a| a.stop_reason)
            .unwrap_or(StopReason::Stop);
        // 被长度截断的消息可能带着半截工具调用，回填给上游会直接报错，
        // 因此只保留正常结束的助手消息。
        if let Some(a) = assistant {
            if stop_reason != StopReason::Length
                && (!calls.is_empty() || !round_text.trim().is_empty())
            {
                messages.push(Message::Assistant(Box::new(a)));
            }
        }
        if !round_text.trim().is_empty() {
            final_text = round_text.clone();
        }
        if interrupted {
            break;
        }
        if let Some(err) = failure {
            core.emit(Event::AssistantText {
                session_id: sid.clone(),
                text: format!("\n[错误] {err}"),
            });
            if final_text.is_empty() {
                final_text = format!("[错误] {err}");
            }
            break;
        }
        // 上游按长度截断：丢弃这一轮不完整的工具调用，换小步继续，
        // 而不是把「半截输出」当成任务完成。
        if stop_reason == StopReason::Length {
            if stall_retries < 2 {
                stall_retries += 1;
                calls.clear();
                messages.push(user_msg(
                    "上一条回复因输出长度限制被截断，任务还没完成。请继续：不要重复已完成的步骤；写文件用 write 工具，内容长的文件拆成多次较小的写入或追加，最后再给简短结论。",
                ));
                continue;
            }
            let msg = "输出连续两次被上游长度限制截断，任务未完成。请把任务拆小后重试。";
            core.emit(Event::AssistantText {
                session_id: sid.clone(),
                text: msg.into(),
            });
            final_text = msg.into();
            break;
        }
        if calls.is_empty() {
            // 既没有正文也没有工具调用，说明模型空转了，不能直接收尾。
            if round_text.trim().is_empty() && stall_retries < 2 {
                stall_retries += 1;
                messages.push(user_msg(
                    "你刚才既没有输出正文，也没有调用工具，任务可能还没完成。请继续执行：需要工具就直接调用；已经完成就用一两句话给出结果和【Voice】播报。",
                ));
                continue;
            }
            if round_text.trim().is_empty() {
                let msg = "模型没有返回内容，任务未完成。请重试，或把任务拆成更小的步骤。";
                core.emit(Event::AssistantText {
                    session_id: sid.clone(),
                    text: msg.into(),
                });
                final_text = msg.into();
            }
            break;
        }

        // 顺序执行工具，保证结果顺序与调用顺序一致。
        for call in calls {
            let tool = tools
                .iter()
                .find(|t| t.schema().name == call.name)
                .cloned();
            let (result_text, ok) = match tool {
                Some(t) => {
                    match t
                        .execute(
                            &call.id,
                            call.arguments.clone(),
                            cancel.child_token(),
                            Arc::new(|_| {}),
                        )
                        .await
                    {
                        Ok(r) => (tool_result_text(&r), true),
                        Err(e) => (format!("工具执行失败: {e}"), false),
                    }
                }
                None => (
                    format!(
                        "工具 {} 当前不可用：渐进式加载下请先调用 ToolGuide 请求该工具。",
                        call.name
                    ),
                    false,
                ),
            };
            let summary: String = result_text.chars().take(300).collect();
            core.emit(Event::ToolResult {
                session_id: sid.clone(),
                agent: "main".into(),
                name: call.name.clone(),
                ok,
                summary: summary.clone(),
            });
            if let Some(rec) = tool_log
                .iter_mut()
                .rev()
                .find(|r| r.name == call.name && !r.done)
            {
                rec.ok = ok;
                rec.summary = summary;
                rec.done = true;
            }
            messages.push(tool_result_msg(&call, &result_text, !ok));
        }

        if turn + 1 == max_turns {
            reached_max = true;
        }
    }
    drop(guard);

    if interrupted {
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
        // 被打断的一轮也要落盘：重新打开会话时保留用户提问与"被打断"占位，
        // 并带上本轮耗时；interrupted=true 让历史重建时跳过它（不进 AI 上下文）。
        let elapsed = t0.elapsed().as_millis() as u64;
        {
            let mut sessions = core.sessions.lock().await;
            sessions.append_full(
                &sid,
                "assistant",
                "被打断",
                tool_log.clone(),
                elapsed,
                true,
            );
        }
        core.emit(Event::AssistantDone {
            session_id: sid.clone(),
            text: "被打断".into(),
            elapsed_ms: elapsed,
            interrupted: true,
        });
    } else {
        if final_text.is_empty() && reached_max {
            final_text = format!("（已达到最大轮数 {max_turns}，本轮结束）");
        }
        if !final_text.is_empty() || !tool_log.is_empty() {
            let mut sessions = core.sessions.lock().await;
            sessions.append(
                &sid,
                "assistant",
                &final_text,
                tool_log.clone(),
                t0.elapsed().as_millis() as u64,
            );
        }
        core.emit(Event::AssistantDone {
            session_id: sid.clone(),
            text: final_text.clone(),
            elapsed_ms: t0.elapsed().as_millis() as u64,
            interrupted: false,
        });
    }

    core.main_busy.store(false, Ordering::SeqCst);
    *core.turn_session.lock().await = None;
    core.set_main_status("idle");

    // 自动语音播报：轮结束后由 core 直接播报最终文本，不依赖 AI 调用语音工具。
    // 被打断的一轮不播报；TTS 在 auto_announce 内部异步执行，不阻塞返回。
    if !interrupted {
        auto_announce(core.clone(), final_text.clone()).await;
    }
}

/// 从最终 Assistant 消息中提取正文（用于不逐段推送的端点）。
fn assistant_text_of(m: &AssistantMessage) -> String {
    m.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 从最终 Assistant 消息中提取思考内容。
fn assistant_thinking_text(m: &AssistantMessage) -> String {
    m.content
        .iter()
        .filter_map(|c| match c {
            Content::Thinking(t) => Some(t.thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 提取工具返回的纯文本内容。
fn tool_result_text(r: &rpi_agent::types::AgentToolResult) -> String {
    r.content
        .iter()
        .map(|c| match c {
            rpi_agent::types::TextContentOrImage::Text(t) => t.text.clone(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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

/// 标记文件路径：记录「当前系统静音是本 App 造成的」。
/// 正常流程会在录音结束后删除；若进程在录音中被杀，标记会留下，
/// 下次启动时据此恢复输出，避免整个 App 变成无声。
static MUTE_MARKER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 启动时调用：初始化标记路径；若上次异常退出遗留静音，先恢复输出。
pub fn init_mute_state(data_dir: &std::path::Path) {
    let marker = data_dir.join(".output_muted");
    let _ = MUTE_MARKER.set(marker.clone());
    if marker.exists() {
        tracing::warn!("检测到上次退出时系统输出仍被静音，启动时恢复");
        system_mute(false);
    }
}

/// 退出前调用：只有静音确实是本 App 造成时才恢复，避免误改用户手动静音。
pub fn restore_if_muted() {
    if matches!(MUTE_MARKER.get(), Some(p) if p.exists()) {
        system_mute(false);
    }
}

/// 系统级静音/恢复：唤醒音播放完毕后立刻压掉系统所有声音（含正在播放的
/// 音乐/视频），录音结束后恢复。用 osascript 控制 macOS 输出，mute/unmute
/// 不改变音量值，恢复时回到原音量。
pub(crate) fn system_mute(muted: bool) {
    if let Some(marker) = MUTE_MARKER.get() {
        if muted {
            let _ = std::fs::write(marker, b"1");
        } else {
            let _ = std::fs::remove_file(marker);
        }
    }
    let script = if muted {
        "set volume with output muted"
    } else {
        "set volume without output muted"
    };
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output();
}

/// 静音状态下录制一轮用户语音：进入即静音，录音结束（无论成功失败）即恢复。
/// 说完后的静默阈值取 silence_limit_secs（默认 1.5s）；
/// 压根没人说话时最多等 no_speech_timeout_secs（默认 4s）就结束、回到监听。
async fn listen_once_muted(core: Arc<CoreState>, max_secs: f64) -> Result<String, anyhow::Error> {
    let (silence_secs, no_speech_secs) = {
        let c = core.config.lock().await;
        (c.silence_limit_secs, c.no_speech_timeout_secs)
    };
    system_mute(true);
    // 用 Drop guard 恢复：即使本轮被取消/超时提前返回，也不会把系统留在静音状态。
    struct RestoreOnDrop;
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            system_mute(false);
        }
    }
    let _restore = RestoreOnDrop;
    core.voice
        .listen_once_silence(max_secs, silence_secs, no_speech_secs)
        .await
}

pub async fn handle_wakeword(core: Arc<CoreState>, keyword: String) {
    // 连续对话模式：本模式自己已在录音（VAD 判说话），唤醒词回调直接丢弃——
    // 否则同一句话会被"常驻唤醒检测"和"连续录音"两路各发一次，重复执行。
    // 注意：语音打断（computer stop）走 voice-serve 的 /api/chat/interrupt，
    // 不经过本函数，所以屏蔽唤醒词不会影响连续模式下的打断能力。
    if core.continuous_active.load(Ordering::SeqCst) {
        tracing::info!("连续对话模式中：忽略唤醒词回调「{keyword}」");
        return;
    }
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
        // 唤醒提示音固定用安装包内的资源，避免旧配置里的绝对路径在升级/移动 App 后失效。
        let beep = crate::paths::resource("wake_sound.wav")
            .to_string_lossy()
            .into_owned();
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
        if let Err(e) = core.voice.beep(Some(&beep)).await {
            tracing::warn!("唤醒提示音播放失败: {e}");
        }
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
            // 语音指令进入连续对话模式：「开启讨论模式」这类话是**纯控制信号**——
            // 本地模糊匹配命中后直接进模式，不把这句发给 AI、也不写进会话历史。
            if is_enter_continuous(&t) {
                core.continuous_active.store(true, Ordering::SeqCst);
                core.emit(Event::Voice {
                    kind: "continuous".into(),
                    text: "start".into(),
                });
                tracing::info!("连续对话模式：用户表达进入「{t}」，仅作控制信号不发给 AI");
                // play_start=true：本轮没有 run_main_turn 播「已发送」音效，
                // 开启音效由 continuous_session 自己播（与结束音效同一个）。
                tokio::spawn(continuous_session(core.clone(), voice_sid, true));
                return;
            }
            // 没命中本地模糊匹配的正常指令：交给 AI（AI 也可用 ContinuousMode 工具自主进入）
            let _ = run_main_turn(core.clone(), t, voice_sid.clone(), false).await;
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
- 回复末尾必须单独一行以【Voice】开头写符合输出语言的播报，不超过100字，口语化，无任何符号，必须完整写出要朗读的句子。
- 回答语言以系统注入的【输出语言】规则为准（默认中文），简洁、快速、专业，像精炼的星舰通讯，正文1~3句。
- 优先使用可用 Skills 完成任务；参考长期记忆中的用户偏好。"
            .to_string(),
    }
}

/// 从 markdown 中移除某个 "## " 小节（到下一个 "## " 标题为止）。
fn strip_section(md: &str, heading: &str) -> String {
    match md.find(heading) {
        Some(idx) => {
            let rest = &md[idx..];
            let end = rest
                .find("\n## ")
                .map(|i| idx + i + 1)
                .unwrap_or(md.len());
            format!("{}{}", &md[..idx], &md[end..]).trim_end().to_string()
        }
        None => md.to_string(),
    }
}

/// 统一组装系统提示词（唯一入口，顺序固定）：
/// 输出语言 → 核心行为规则 → 个人设定 → 可用 Skills → 长期记忆。
/// 空段自动跳过，避免反复追加产生重复规则或"（暂无…）"噪音。
pub fn compose_system_prompt(cfg: &Config, skills_text: &str, memory_text: &str) -> String {
    let mut rules = load_system_rules();
    if !cfg.ask_user_enabled {
        // 关闭中途询问时移除该节，避免模型调用未注册工具；其余规则保持不变
        rules = strip_section(&rules, "## 3. 主动询问用户");
    }
    let mut parts: Vec<String> = vec![language_directive(&cfg.output_language)];
    parts.push(format!(
        "=== 核心行为规则（系统级，优先于下面的个人设定） ===\n{}",
        rules.trim()
    ));
    let persona = cfg.main_system_prompt.trim();
    if !persona.is_empty() {
        parts.push(format!("=== 个人设定（用户配置的人设） ===\n{persona}"));
    }
    let skills = skills_text.trim();
    if !skills.is_empty() && !skills.starts_with("（暂无") {
        parts.push(format!("=== 可用 Skills ===\n{skills}"));
    }
    let memory = memory_text.trim();
    if !memory.is_empty() && !memory.starts_with("（暂无") {
        parts.push(format!("=== 长期记忆 ===\n{memory}"));
    }
    parts.join("\n\n")
}

/// 输出语言强制指令：拼接在系统提示词最前，声明最高优先级，
/// 覆盖人设（System Prompt）与其它章节中的任何语言要求。
/// zh/en 强制该语言输出（正文与【Voice】播报）；auto 要求跟随用户输入语言。
pub fn language_directive(lang: &str) -> String {
    match lang {
        "en" => "## 0. 输出语言（最高优先级，覆盖人设与其它章节中的任何语言要求）\n\
- Your entire reply MUST be written in English only; do not mix in Chinese sentences.\n\
- The 【Voice】 announcement line MUST also be in English only.\n\
- If the persona (System Prompt) or any other section asks for Chinese, ignore it and follow this section.\n\
- Keep the 【Voice】 marker line format exactly as required below."
            .to_string(),
        "auto" => "## 0. 输出语言（最高优先级）\n\
- 用与用户输入相同的语言回答；无法判断时默认用中文。\n\
- 【Voice】播报段使用与正文相同的语言。"
            .to_string(),
        _ => "## 0. 输出语言（最高优先级，覆盖人设与其它章节中的任何语言要求）\n\
- 你的正文必须【全部使用中文】，不要整句英文；专有名词可保留原文。\n\
- 【Voice】播报段同样必须【全部使用中文】。\n\
- 若人设（System Prompt）或其它章节要求用英文，一律以本节为准，忽略之。"
            .to_string(),
    }
}

// ============================================================
// 连续对话模式（多轮讨论）
// 进入后无需再喊唤醒词：每轮用 VAD 判"有没有人在说话"，说完静默 1.5s 即发送；
// AI 还在生成/调工具时用户开口 → 先打断（先打断、再发送）；
// 用户说结束语 / AI 调 ContinuousMode 工具 / 静默连续数轮 → 退出。
// ============================================================

/// 文本归一化：去掉空白与标点、全角标点丢弃，便于模糊匹配口头指令。
fn normalize_utterance(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_whitespace() {
            continue;
        }
        match ch {
            '，' | '。' | '、' | '！' | '？' | '；' | '：' | '“' | '”' | '‘' | '’' | '（' | '）'
            | '《' | '》' | '【' | '】' | '…' | '·' | '～' | ',' | '.' | '!' | '?' | ';' | ':'
            | '"' | '\'' | '(' | ')' | '~' => continue,
            _ => out.push(ch),
        }
    }
    out.to_lowercase()
}

/// 明确表达"开启连续/多轮对话"的说法（归一化后包含匹配）
const ENTER_PHRASES: &[&str] = &[
    "多轮对话",
    "多轮模式",
    "多轮讨论",
    "连续对话",
    "连续模式",
    "持续对话",
    "讨论模式",
    "对话模式",
    "聊天模式",
    "开启对话",
    "开始对话",
    "进入对话",
];

/// 短语模糊命中：允许用户在短语前后多说几个字，但要求短语基本覆盖整句。
/// 这样「先这样吧」「我们结束讨论吧」能命中，而「帮我关闭对话窗口」不会误判。
fn phrase_hits(t: &str, phrases: &[&str]) -> bool {
    let n = t.chars().count();
    phrases
        .iter()
        .any(|w| t.contains(w) && n <= w.chars().count() + 3)
}

/// 判断用户这句话是否是"开启连续对话模式"的指令（模糊识别，不依赖 NLU 模型）
pub fn is_enter_continuous(text: &str) -> bool {
    let t = normalize_utterance(text);
    // 长句多半是正常内容而不是模式指令，只对短句做判定，避免误触发
    if t.chars().count() > 20 {
        return false;
    }
    if phrase_hits(&t, ENTER_PHRASES) {
        return true;
    }
    // 「开启/打开/进入/开始/启动/切换」+「对话/讨论/多轮/连续/聊天」组合
    const VERBS: &[&str] = &["开启", "打开", "进入", "开始", "启动", "启用", "切换", "来个"];
    const NOUNS: &[&str] = &["对话", "讨论", "多轮", "连续", "聊天"];
    VERBS.iter().any(|v| t.contains(v)) && NOUNS.iter().any(|n| t.contains(n))
}

/// 明确表达"可以结束了"的说法（归一化后包含匹配）。
/// 刻意不收裸词「结束」「停止」，避免把"帮我结束这个进程"这类正常指令误判为退出。
const EXIT_PHRASES: &[&str] = &[
    "结束对话",
    "结束讨论",
    "结束多轮",
    "结束连续",
    "结束模式",
    "退出对话",
    "退出讨论",
    "退出模式",
    "退出多轮",
    "关闭对话",
    "关闭模式",
    "关闭多轮",
    "停止对话",
    "取消对话",
    "到此为止",
    "就到这",
    "先这样",
    "就这样吧",
    "可以结束了",
    "不用了",
    "不聊了",
    "不想聊了",
    "聊到这",
    "结束吧",
    "结束啦",
    "不用继续了",
    "不继续了",
    "退下",
];

/// 判断用户这句话是否是"结束连续对话模式"的意图（模糊识别，本地优先；不命中才交给 AI）
pub fn is_exit_continuous(text: &str) -> bool {
    let t = normalize_utterance(text);
    if t.chars().count() > 18 {
        return false;
    }
    if phrase_hits(&t, EXIT_PHRASES) {
        return true;
    }
    // 组合匹配：「关闭/结束/退出/停止/取消」+「讨论/多轮/连续/聊天/模式」。
    // 覆盖「关闭讨论模式」「结束多轮对话」这类中间夹字的说法——整串不在固定表里也能命中。
    // 名词刻意不含裸「对话」，避免把"关闭对话窗口"这类正常指令误判成退出。
    const VERBS: &[&str] = &["关闭", "结束", "退出", "停止", "取消", "别聊"];
    const NOUNS: &[&str] = &["讨论", "多轮", "连续", "聊天", "模式"];
    VERBS.iter().any(|v| t.contains(v)) && NOUNS.iter().any(|n| t.contains(n))
}

/// 连续对话模式音效：多轮讨论的「开启 / 结束 / 中间发送」统一使用项目内固定音效
/// `resources/computerbeep_29.mp3`（相对项目根解析，不再扫描下载文件夹，避免被下载夹里
/// 其他音频顶掉）。文件缺失时返回 None（调用方回退到配置的提示音）。
fn continuous_effect_path() -> Option<String> {
    let p = crate::paths::resource("computerbeep_29.mp3");
    if p.is_file() {
        Some(p.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// 播放连续对话模式音效（项目内固定音效；找不到则用配置提示音）
async fn play_continuous_effect(core: Arc<CoreState>) {
    let path = continuous_effect_path().or_else(|| {
        let beep = core
            .config
            .try_lock()
            .map(|c| c.beep_file.clone())
            .unwrap_or_default();
        if beep.is_empty() {
            None
        } else {
            Some(beep)
        }
    });
    if let Some(p) = path {
        let _ = core.voice.beep(Some(&p)).await;
    }
}

/// 由前端按钮/AI 工具开启连续对话模式（要求监听已开启；幂等）
pub async fn start_continuous(core: Arc<CoreState>) {
    if core.continuous_active.swap(true, Ordering::SeqCst) {
        return; // 已在连续模式
    }
    let sid = core.voice_session.lock().await.clone();
    tokio::spawn(continuous_session(core, sid, true));
}

/// 连续对话（多轮讨论）主循环。
/// play_start=true 时在进入时播放开启音效（按钮路径已无本轮对话，需要自己播）。
pub async fn continuous_session(core: Arc<CoreState>, sid: Option<String>, play_start: bool) {
    if play_start {
        play_continuous_effect(core.clone()).await;
    }
    // 时长参数全部来自设置（设置页「语音」分组可调）
    let (max_record, silence_secs, idle_rounds, no_speech_secs) = {
        let c = core.config.lock().await;
        (
            c.continuous_max_record_secs.clamp(3.0, 60.0),
            c.continuous_silence_secs.clamp(0.6, 5.0),
            c.continuous_idle_rounds.max(1),
            c.no_speech_timeout_secs.clamp(1.0, 30.0),
        )
    };
    let mut idle: u32 = 0;
    tracing::info!("连续对话模式：进入循环");
    while core.continuous_active.load(Ordering::SeqCst) {
        // ① AI 正在播报（TTS 出声）期间不开麦：本机没有回声消除，此时开麦会把
        //    扬声器里 AI 自己的声音当成用户输入，导致 AI 自己打断自己。
        for _ in 0..600 {
            if !core.continuous_active.load(Ordering::SeqCst)
                || !core.speaking.load(Ordering::SeqCst)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !core.continuous_active.load(Ordering::SeqCst) {
            break;
        }

        // ② 直接开麦录这一句（不再先探测人声起点——探测会把句首吃掉）。
        //    AI 在生成/调工具（main_busy，尚未出声）期间也照样录：
        //    用户中途插话时先把整句录完，再在「发送那一刻」打断原来的 AI 轮次。
        //    录音期间不做整机静音，而是让 voice-serve 忽略「本机 AI 播报」那部分音频
        //    （软件回音门），所以 AI 的回答照常出声，且不会被麦克风收回来当用户输入。
        let text = core
            .voice
            .listen_once_gated(max_record, silence_secs, no_speech_secs)
            .await;
        if !core.continuous_active.load(Ordering::SeqCst) {
            break;
        }
        match text {
            Ok(t) if !t.trim().is_empty() => {
                let t = t.trim().to_string();
                idle = 0;
                core.emit(Event::Voice {
                    kind: "stt".into(),
                    text: t.clone(),
                });
                if is_exit_continuous(&t) {
                    // 用户明确表达结束：直接退出，省一轮往返
                    tracing::info!("连续对话模式：用户表达结束「{t}」");
                    break;
                }
                // ③ 发送那一刻：若上一轮 AI 还在生成/调工具/播报，先打断再发送，
                //    让新指令立即接管（"AI 工作中插话有反应"的关键）。
                if core.main_busy.load(Ordering::SeqCst) || core.speaking.load(Ordering::SeqCst) {
                    let _ = core.voice.interrupt().await;
                    core.interrupt_main.store(true, Ordering::SeqCst);
                    core.emit(Event::Voice {
                        kind: "continuous".into(),
                        text: "interrupt".into(),
                    });
                    // 等被打断的一轮真正收尾（run_main_turn 每 50ms 检查打断标志），
                    // 否则它的 TTS 可能在我们下一轮录音期间响起来、被麦克风收进去。
                    for _ in 0..60 {
                        if !core.main_busy.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    // 余量：interrupt 是异步的，留一点时间让残留播放真正停下
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if !core.continuous_active.load(Ordering::SeqCst) {
                    break;
                }
                // ④ 不 await：本轮后台跑，循环立刻回到顶部继续录音，
                //    这样 AI 工作期间也随时能接住用户的下一句（barge-in）。
                tokio::spawn(run_main_turn(core.clone(), t, sid.clone(), false));
                // 让新轮次先把 main_busy 置位，避免紧随其后的录音把它误判成空闲
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            _ => {
                // 空录音：仅当 AI 空闲时才计入「没人说话」，
                // 否则长时间任务会被误判成静默而自动退出。
                if !core.main_busy.load(Ordering::SeqCst) {
                    idle += 1;
                    if idle >= idle_rounds {
                        tracing::info!("连续对话模式：连续 {idle} 轮没人说话，自动退出");
                        break;
                    }
                }
            }
        }
    }
    core.continuous_active.store(false, Ordering::SeqCst);
    // 退出时若还有轮次在跑，一并打断，避免"模式已退出但 AI 还在播报"
    core.interrupt_main.store(true, Ordering::SeqCst);
    let _ = core.voice.interrupt().await;
    // 结束音效（与开启音效同一个）
    play_continuous_effect(core.clone()).await;
    core.emit(Event::Voice {
        kind: "continuous".into(),
        text: "end".into(),
    });
    tracing::info!("连续对话模式：已退出");
}

#[cfg(test)]
mod continuous_phrase_tests {
    use super::{is_enter_continuous, is_exit_continuous};

    #[test]
    fn enter_phrases_hit() {
        for t in [
            "开启讨论模式",
            "打开多轮对话",
            "进入连续对话模式",
            "我们开始讨论模式吧",
            "切换成聊天模式",
            "启用连续对话",
        ] {
            assert!(is_enter_continuous(t), "应识别为进入：{t}");
        }
    }

    #[test]
    fn exit_phrases_hit() {
        for t in [
            "关闭讨论模式",
            "结束讨论",
            "退出多轮对话",
            "可以结束了",
            "先这样吧",
            "不聊了",
            "我不想聊了",
            "到此为止",
            "退下",
            "停止连续",
        ] {
            assert!(is_exit_continuous(t), "应识别为退出：{t}");
        }
    }

    #[test]
    fn normal_commands_not_matched() {
        // 正常指令不能被误判成模式开关（否则会被静默吞掉）
        for t in [
            "帮我关闭对话窗口",
            "结束这个进程",
            "打开一下天气",
            "讨论一下这个方案的优缺点",
            "帮我查一下明天北京的天气怎么样啊",
        ] {
            assert!(!is_enter_continuous(t), "不应识别为进入：{t}");
            assert!(!is_exit_continuous(t), "不应识别为退出：{t}");
        }
    }
}
