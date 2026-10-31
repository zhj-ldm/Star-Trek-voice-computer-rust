//! Agent 编排：构建主 Agent、主 Agent 会话轮、
//! 唤醒词处理与自动语音汇报。主 Agent 只开放搜索+派发+监控+语音工具。

use crate::events::Event;
use crate::search::WebSearchTool;
use crate::sessions::ToolCallRecord;
use crate::state::CoreState;
use crate::tools::{auto_announce, ContinuousMode, DeliverFiles, ImportSkill};
use open_agent_sdk::tools::askuser::{AskUserFn, AskUserTool};
use open_agent_sdk::utils::messages::{create_assistant_message, create_user_message};
use open_agent_sdk::{Agent, AgentOptions, ApiClient, ContentBlock, Message, SDKMessage};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub type MainAgent = Agent;

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
                if ap.model.trim().is_empty() {
                    &cfg.main_model
                } else {
                    &ap.model
                },
                &ap.base_url,
                cfg.enable_thinking,
                &core.ollama_models_cache,
            )
            .await;
            for key in keys {
                let mut c =
                    ApiClient::new(Some(key), Some(ap.base_url.clone()), Some(model.clone()));
                c.set_rpm_limit(ap.rpm_limit.unwrap_or(cfg.rpm_limit));
                c.set_enable_thinking(cfg.enable_thinking);
                clients.push(c);
            }
        }
        if clients.is_empty() {
            return Err(
                "providers 列表为空或全部 base_url 为空，请在设置中配置至少一个 API".into(),
            );
        }
        opts.api_clients = clients;
    } else {
        opts.model = Some(
            resolve_model_name(
                &cfg.main_model,
                &cfg.main_base_url,
                cfg.enable_thinking,
                &core.ollama_models_cache,
            )
            .await,
        );
        opts.api_key = Some(cfg.main_api_key.clone());
        opts.base_url = Some(cfg.main_base_url.clone());
        opts.rpm_limit = Some(cfg.rpm_limit);
        opts.enable_thinking = Some(cfg.enable_thinking);
    }
    opts.cwd = Some(home_dir());
    // 输出语言强制指令置于最前：声明最高优先级，覆盖人设与其它章节中的任意语言要求。
    let lang_directive = language_directive(&cfg.output_language);
    opts.system_prompt = Some(format!(
        "{}\n\n{}\n\n{}",
        lang_directive, system_rules, cfg.main_system_prompt
    ));
    opts.append_system_prompt = Some(append);
    opts.max_turns = Some(cfg.max_turns.min(100));
    // 全部功能：导入 Skill + 真实 WebSearch（覆盖 SDK 占位版）+ 中途语音询问；
    // 其余 bash/文件/WebFetch/AskUser/Tasks 等由 SDK 默认注册，全部开放。
    // 语音播报不再注册为 AI 工具：每轮结束后由 core 自动用最终文本 TTS 播报。
    let mut custom_tools: Vec<Arc<dyn open_agent_sdk::Tool>> = vec![
        Arc::new(ImportSkill::new(core.clone())),
        Arc::new(WebSearchTool::default()),
        Arc::new(DeliverFiles),
        // 连续对话（多轮讨论）模式开关：AI 可自主开启/结束
        Arc::new(ContinuousMode::new(core.clone())),
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
                    (cfg.effective_voice(), cfg.rate)
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
    // OpenCode CLI 模式：本轮由本机 opencode-cli 执行，不构建内置 Agent
    if crate::opencode::active_target(core).await.is_some() {
        core.agents_ready.store(true, Ordering::SeqCst);
        core.set_main_status("idle");
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
    // 先取历史快照（不含本轮输入），OpenCode 分支不使用它，内置 Agent 分支用它覆盖上下文。
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

    // OpenCode CLI 模式：本轮不进内置 Agent，交给本机 opencode-cli 子进程执行。
    // 用户消息已落盘、提示音已播；CLI 的 JSON 事件流会映射成同一套 Event
    //（文本快照 / 工具卡 / 完成），前端无需区分两种后端。
    if let Some(target) = crate::opencode::active_target(&core).await {
        crate::opencode::run_turn(core.clone(), sid.clone(), user_text.clone(), target, t0).await;
        return;
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
    // 无条件覆盖：新会话无历史时清空残留，避免跨会话上下文串扰
    agent.messages = history;

    let (mut rx, handle, cancel_token) = agent.query(&user_text).await;

    let mut _final_text = String::new();
    let mut interrupted = false;
    let mut persisted = false; // assistant 是否已在 Result 分支落盘
                               // 本轮工具调用记录（随 assistant 消息持久化，供切换会话后恢复渲染）
    let mut tool_log: Vec<ToolCallRecord> = Vec::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(30)) => {
                if core.interrupt_main.load(Ordering::SeqCst) {
                    interrupted = true;
                    cancel_token.cancel(); // 让 run_loop 的 select! 立即被触发
                    handle.abort();        // 兜底：强制 cancel 整个 task
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
                    Some(SDKMessage::ReasoningDelta { text }) => {
                        // 流式思考增量：直接转发给前端，前端 addReasoningCard 会累积
                        core.emit(Event::ReasoningText {
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
                            sessions.append(
                                &sid,
                                "assistant",
                                &_final_text,
                                tool_log.clone(),
                                t0.elapsed().as_millis() as u64,
                            );
                        }
                        persisted = true;
                        core.emit(Event::AssistantDone {
                            session_id: sid.clone(),
                            text: _final_text.clone(),
                            elapsed_ms: t0.elapsed().as_millis() as u64,
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
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
        // 被打断的一轮不持久化 assistant（避免把"已打断"占位写成真实历史）
    } else if !persisted {
        // 兜底：rx 异常关闭未走到 Result 分支时，已有内容仍落盘
        if !_final_text.is_empty() || !tool_log.is_empty() {
            let mut sessions = core.sessions.lock().await;
            sessions.append(
                &sid,
                "assistant",
                &_final_text,
                tool_log,
                t0.elapsed().as_millis() as u64,
            );
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
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output();
}

/// 静音状态下录制一轮用户语音：进入即静音，录音结束（无论成功失败）即恢复。
/// 静默结束阈值取自设置（silence_limit_secs，默认 2.5s）。
async fn listen_once_muted(core: Arc<CoreState>, max_secs: f64) -> Result<String, anyhow::Error> {
    let silence_secs = core.config.lock().await.silence_limit_secs;
    system_mute(true);
    let r = core.voice.listen_once_silence(max_secs, silence_secs).await;
    system_mute(false);
    r
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
    let (max_record, silence_secs, idle_rounds) = {
        let c = core.config.lock().await;
        (
            c.continuous_max_record_secs.clamp(3.0, 60.0),
            c.continuous_silence_secs.clamp(0.6, 5.0),
            c.continuous_idle_rounds.max(1),
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
            .listen_once_gated(max_record, silence_secs)
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
