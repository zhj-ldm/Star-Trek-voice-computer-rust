//! HTTP API（axum）：core 与 Electron UI / voice-serve 的通信入口。
//! UI 与后端接口一一对应，新增接口需同步前端。

use crate::agents::{handle_wakeword, rebuild_agents, run_main_turn};
use crate::events::Event;
use crate::state::CoreState;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::{
    extract::{Path, Request, State},
    middleware::{self, Next},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::{delete, get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub type SharedState = Arc<CoreState>;

/// 只放行本机 UI（Electron file:// 的 `null` origin）与本机调试页。
/// 不能返回 `*`：任意网页都能借此读取本地 API 响应（chat 可触发工具执行）。
fn origin_allowed(origin: &str) -> bool {
    let o = origin.trim();
    o == "null"
        || o == "file://"
        || o.starts_with("http://127.0.0.1:")
        || o.starts_with("http://localhost:")
        || o.starts_with("http://[::1]:")
}

fn request_origin(req: &Request) -> Option<String> {
    req.headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn cors_layer(req: Request, next: Next) -> axum::response::Response {
    let origin = request_origin(&req);
    if req.method() == Method::OPTIONS {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        match origin.as_deref() {
            Some(o) if origin_allowed(o) => {
                *resp.status_mut() = StatusCode::NO_CONTENT;
                set_cors(resp.headers_mut(), o);
            }
            Some(_) => *resp.status_mut() = StatusCode::FORBIDDEN,
            None => *resp.status_mut() = StatusCode::NO_CONTENT,
        }
        return resp;
    }
    let mut resp = next.run(req).await;
    if let Some(o) = origin.as_deref().filter(|o| origin_allowed(o)) {
        set_cors(resp.headers_mut(), o);
    }
    resp
}

fn set_cors(h: &mut axum::http::HeaderMap, origin: &str) {
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert("Access-Control-Allow-Origin", v);
    }
    h.insert("Vary", HeaderValue::from_static("Origin"));
    h.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET,POST,DELETE,OPTIONS"),
    );
    h.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("Content-Type"),
    );
}

pub fn router(core: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/api/events", get(events_sse))
        .route("/api/chat", post(chat))
        .route("/api/chat/interrupt", post(chat_interrupt))
        .route("/api/voice/wakeword", post(voice_wakeword))
        .route("/api/voice/beep", post(voice_beep))
        .route("/api/voice/listening", post(voice_listening))
        .route("/api/voice/session", post(voice_session))
        .route("/api/voice/continuous", post(voice_continuous))
        .route("/api/voice/interrupt", post(voice_interrupt))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/skills", get(list_skills))
        .route("/api/skills/import", post(import_skill))
        .route("/api/skills/:name", delete(remove_skill))
        .route("/api/skills/:name/toggle", post(toggle_skill))
        .route("/api/mcp/servers", get(list_mcp).post(add_mcp))
        .route("/api/mcp/servers/:name", delete(remove_mcp))
        .route("/api/mcp/servers/:name/test", post(test_mcp))
        .route("/api/memory", get(list_memory).post(add_memory))
        .route("/api/memory/:id", delete(remove_memory).put(update_memory))
        .route("/api/memory/clear", post(clear_memory))
        .route("/api/schedules", get(list_schedules).post(add_schedule))
        .route(
            "/api/schedules/:id",
            post(update_schedule).delete(remove_schedule),
        )
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/:id", delete(delete_session))
        .route("/api/sessions/:id/rename", post(rename_session))
        .route("/api/sessions/:id/switch", post(switch_session))
        .route("/api/sessions/:id/messages", get(get_session_messages))
        .route(
            "/api/sessions/:id/messages/:mid",
            delete(delete_session_message),
        )
        .route("/api/sessions/:id/clear", post(clear_session))
        .route("/api/tools", get(list_tools).post(save_tool_defaults))
        .route("/api/subagents", get(list_subagents))
        .route("/api/subagents/purge", post(purge_subagents))
        .route(
            "/api/subagents/:id",
            get(get_subagent).delete(delete_subagent),
        )
        .route("/api/subagents/:id/cancel", post(cancel_subagent))
        .route("/api/subagents/:id/message", post(message_subagent))
        .route("/api/admin/rebuild", post(admin_rebuild))
        .route("/api/system/open-mic-settings", post(open_mic_settings))
        .route("/api/system/reveal", post(reveal_path))
        .layer(middleware::from_fn(cors_layer))
        .with_state(core)
}

// ---------- 基础 ----------

async fn health() -> &'static str {
    "star-core ok"
}

async fn status(State(core): State<SharedState>) -> Json<Value> {
    let cfg = core.config.lock().await;
    let voice_connected = core.voice.health().await;
    // voice-serve 语音链路诊断（麦克风权限 / 采集 / KWS 模型）
    let mut voice_diag = serde_json::Map::new();
    if voice_connected {
        if let Ok(v) = core.voice.status().await {
            if let Some(obj) = v.as_object() {
                for (k, val) in obj {
                    voice_diag.insert(k.clone(), val.clone());
                }
            }
        }
    }
    let status = json!({
        "main_status": if core.main_busy.load(Ordering::SeqCst) {"working"} else {"idle"},
        "turn_session": core.turn_session.lock().await.clone(),
        "speaking": core.speaking.load(Ordering::SeqCst),
        "voice_active": core.voice_active.load(Ordering::SeqCst),
        "continuous_active": core.continuous_active.load(Ordering::SeqCst),
        "agents_ready": core.agents_ready.load(Ordering::SeqCst),
        "voice_connected": voice_connected,
        "voice_enabled": cfg.voice_enabled,
        "voice_port": cfg.voice_port,
        "wakeword": cfg.wakeword,
        "mic_alive": voice_diag.get("mic_alive").and_then(|v| v.as_bool()).unwrap_or(false),
        "voice_diag": serde_json::Value::Object(voice_diag),
    });
    Json(status)
}

// ---------- SSE 事件流 ----------

/// SSE：带事件序号 + Last-Event-ID 重放。
/// 断线重连时浏览器自动带上 Last-Event-ID，从环形缓冲补齐缺口；
/// 广播 Lagged 不再断流，而是从缓冲补发；缓冲也补不齐时发 resync 让前端整体刷新。
async fn events_sse(
    State(core): State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let last_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    // 先读序号再订阅：若先 subscribe 再读，中间产生的事件 id 会 <= last，
    // 被下面的去重逻辑当成重复事件丢掉，新连接就永久缺失这几条。
    let seq_at_subscribe = core.event_seq.load(Ordering::SeqCst);
    let rx = core.events.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        // 无 Last-Event-ID = 全新连接：只推后续事件，不重放历史
        let mut last = last_id.unwrap_or(seq_at_subscribe);
        let seq_now = core.event_seq.load(Ordering::SeqCst);
        if last > seq_now {
            // 后端重启后序号从 0 重新计：客户端带来的旧 id 没有意义。
            // 归零后按环形缓冲全量重放（并触发 gap→resync），否则重启到重连
            // 之间产生的事件既不在重放区间、也不会被判为缺口，会静默丢失。
            last = 0;
        }
        let mut need_replay = true;
        loop {
            if need_replay {
                need_replay = false;
                // 从环形缓冲补齐 last 之后的事件（重连重放 / Lagged 补发共用）
                let (pending, gap) = {
                    let log = core
                        .event_log
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let front = log.front().map(|(id, _)| *id).unwrap_or(last + 1);
                    let gap = front > last + 1;
                    let pending: Vec<(u64, Event)> = log
                        .iter()
                        .filter(|(id, _)| *id > last)
                        .cloned()
                        .collect();
                    (pending, gap)
                };
                if gap {
                    let data = serde_json::to_string(&Event::Resync).unwrap_or_default();
                    yield Ok(SseEvent::default().data(data));
                }
                for (id, ev) in pending {
                    last = id;
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    yield Ok(SseEvent::default().id(id.to_string()).data(data));
                }
            }
            match rx.recv().await {
                Ok((id, ev)) => {
                    if id <= last {
                        continue; // 重放已覆盖，去重
                    }
                    last = id;
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    yield Ok(SseEvent::default().id(id.to_string()).data(data));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 消费太慢丢了一截：从环形缓冲补齐，不断流
                    need_replay = true;
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------- 对话 ----------

#[derive(Deserialize)]
struct ChatReq {
    text: String,
    #[serde(default)]
    session_id: Option<String>,
}

async fn chat(State(core): State<SharedState>, Json(req): Json<ChatReq>) -> Json<Value> {
    let text = req.text.trim().to_string();
    if text.is_empty() {
        return Json(json!({"ok": false, "error": "空消息"}));
    }
    if core.main_busy.load(Ordering::SeqCst) {
        return Json(json!({"ok": false, "error": "主 Agent 正在处理中，可语音说 stop 打断"}));
    }
    let core2 = core.clone();
    tokio::spawn(async move {
        run_main_turn(core2, text, req.session_id, false).await;
    });
    Json(json!({"ok": true}))
}

async fn chat_interrupt(State(core): State<SharedState>) -> Json<Value> {
    // 只有确实有轮次在跑才置打断标志：空闲时置位会残留，误杀下一个新任务
    if core.main_busy.load(Ordering::SeqCst) {
        core.interrupt_main.store(true, Ordering::SeqCst);
    }
    let _ = core.voice.interrupt().await;
    Json(json!({"ok": true}))
}

// ---------- 多会话 ----------

async fn list_sessions(State(core): State<SharedState>) -> Json<Value> {
    let sessions = core.sessions.lock().await;
    let active = sessions.active_id();
    let list = sessions
        .list()
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "created_at": s.created_at,
                "updated_at": s.updated_at,
                "message_count": s.messages.len(),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({"ok": true, "active": active, "sessions": list}))
}

#[derive(Deserialize)]
struct SessionReq {
    #[serde(default)]
    title: Option<String>,
}

async fn create_session(
    State(core): State<SharedState>,
    Json(req): Json<SessionReq>,
) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let s = sessions.create(req.title.as_deref());
    Json(json!({"ok": true, "id": s.id, "title": s.title}))
}

async fn delete_session(State(core): State<SharedState>, Path(id): Path<String>) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.delete(&id);
    Json(json!({"ok": ok}))
}

async fn rename_session(
    State(core): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<SessionReq>,
) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.rename(&id, req.title.unwrap_or_default().as_str());
    Json(json!({"ok": ok}))
}

async fn switch_session(State(core): State<SharedState>, Path(id): Path<String>) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.switch(&id);
    drop(sessions);
    Json(json!({"ok": ok}))
}

async fn get_session_messages(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let sessions = core.sessions.lock().await;
    let msgs = sessions.get(&id).map(|s| s.messages).unwrap_or_default();
    Json(json!({"ok": true, "messages": msgs}))
}

async fn clear_session(State(core): State<SharedState>, Path(id): Path<String>) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.clear_messages(&id);
    Json(json!({"ok": ok}))
}

/// 删除某一轮问答（用户消息及其后续助手回复），同时从历史上下文剔除
async fn delete_session_message(
    State(core): State<SharedState>,
    Path((id, mid)): Path<(String, String)>,
) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.delete_turn(&id, &mid);
    Json(json!({"ok": ok}))
}

/// 渐进式加载：列出全部懒加载工具及其默认加载开关状态
async fn list_tools(State(core): State<SharedState>) -> Json<Value> {
    let (loading, defaults) = {
        let cfg = core.config.lock().await;
        (cfg.tool_loading.clone(), cfg.default_loaded_tools.clone())
    };
    let hub = crate::tools::ToolHub::new(core.clone());
    let tools: Vec<Value> = hub
        .lazy_schemas()
        .into_iter()
        .map(|t| json!({"name": t.name, "description": t.description}))
        .collect();
    Json(json!({
        "ok": true,
        "loading": loading,
        "defaults": defaults,
        "tools": tools
    }))
}

/// 子 Agent 任务列表（前端「子 Agent」页与完成卡片使用）
async fn list_subagents(State(core): State<SharedState>) -> Json<Value> {
    Json(json!({ "tasks": core.subagents.list().await }))
}

/// 取消一个运行中的子 Agent
async fn cancel_subagent(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let ok = core.subagents.cancel(&id).await;
    Json(json!({ "ok": ok }))
}

/// 单个子 Agent 详情（含执行轨迹），供全屏详情页使用
async fn get_subagent(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match core.subagents.get(&id).await {
        Some(task) => Ok(Json(json!({ "ok": true, "task": task }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "任务不存在" })),
        )),
    }
}

#[derive(Deserialize)]
struct SubagentMessageReq {
    text: String,
}

/// 给子 Agent 追加指令：运行中排队，已结束则带着原上下文继续跑
async fn message_subagent(
    State(core): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<SubagentMessageReq>,
) -> Json<Value> {
    match core
        .subagents
        .send_instruction(core.clone(), &id, &req.text)
        .await
    {
        Ok(queued) => Json(json!({ "ok": true, "queued": queued })),
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

#[derive(Deserialize, Default)]
struct DeleteSubagentQuery {
    #[serde(default)]
    with_conversation: bool,
}

/// 删除单条任务记录（可选同时删除对话卡片）
async fn delete_subagent(
    State(core): State<SharedState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteSubagentQuery>,
) -> Json<Value> {
    let ok = core
        .subagents
        .delete(&core, &id, q.with_conversation)
        .await;
    Json(json!({ "ok": ok }))
}

#[derive(Deserialize, Default)]
struct PurgeSubagentsReq {
    #[serde(default)]
    before_ms: i64,
    #[serde(default)]
    with_conversation: bool,
}

/// 按时间批量清理已结束的任务记录（before_ms<=0 = 全部）
async fn purge_subagents(
    State(core): State<SharedState>,
    Json(req): Json<PurgeSubagentsReq>,
) -> Json<Value> {
    let removed = core
        .subagents
        .purge(&core, req.before_ms, req.with_conversation)
        .await;
    Json(json!({ "ok": true, "removed": removed.len() }))
}

#[derive(Deserialize)]
struct ToolDefaultsReq {
    #[serde(default)]
    names: Vec<String>,
}

/// 保存「默认加载」工具集合（逐个开关），随后重建 Agent 让设置立即生效
async fn save_tool_defaults(
    State(core): State<SharedState>,
    Json(req): Json<ToolDefaultsReq>,
) -> Json<Value> {
    {
        let mut cfg = core.config.lock().await;
        cfg.default_loaded_tools = req.names;
        if let Err(e) = cfg.save(&core.config_path) {
            return Json(json!({"ok": false, "error": format!("保存失败: {e}")}));
        }
    }
    let core2 = core.clone();
    tokio::spawn(async move {
        rebuild_agents(&core2).await;
        core2.emit(Event::SettingsUpdated);
    });
    Json(json!({"ok": true}))
}

// ---------- 语音 ----------

#[derive(Deserialize)]
struct WakewordReq {
    keyword: String,
}

async fn voice_wakeword(
    State(core): State<SharedState>,
    Json(req): Json<WakewordReq>,
) -> Json<Value> {
    let core2 = core.clone();
    tokio::spawn(async move {
        handle_wakeword(core2, req.keyword).await;
    });
    Json(json!({"ok": true}))
}

#[derive(Deserialize)]
struct ListeningReq {
    enabled: bool,
    #[serde(default)]
    session_id: Option<String>,
}

async fn voice_listening(
    State(core): State<SharedState>,
    Json(req): Json<ListeningReq>,
) -> Json<Value> {
    // 记录语音对话目标会话：开启监听时前端传入当前会话，关闭时清空
    let session_id = if req.enabled { req.session_id } else { None };
    *core.voice_session.lock().await = session_id.clone();
    let ok = core.voice.set_listening(req.enabled).await.is_ok();
    // 监听关闭 = 主语音链路下线：连续对话模式必须一并退出，
    // 否则循环会继续录音、而它在业务上已失去存在前提。
    if ok && !req.enabled {
        core.continuous_active.store(false, Ordering::SeqCst);
    }
    // 持久化监听状态：重启 App 后 core 重新拉起 voice-serve 时据此恢复，
    // 避免"已开启监听但重启后按钮颜色不恢复 / 监听静默丢失"。
    if ok {
        let data_dir = core.config.lock().await.data_dir.clone();
        let path = data_dir.join("voice-state.json");
        let state = serde_json::json!({
            "listening": req.enabled,
            "session_id": session_id,
        });
        if let Ok(s) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(&path, s);
        }
    }
    Json(json!({"ok": ok}))
}

#[derive(Deserialize)]
struct VoiceSessionReq {
    #[serde(default)]
    session_id: Option<String>,
}

/// 仅更新"语音对话目标会话"，不改变监听开关。
/// 前端每次切换/新建/删除会话都会调用，保证唤醒后的话落在当前正在看的会话里。
async fn voice_session(
    State(core): State<SharedState>,
    Json(req): Json<VoiceSessionReq>,
) -> Json<Value> {
    *core.voice_session.lock().await = req.session_id.clone();
    // 持久化目标会话，重启后与监听状态一起恢复
    let data_dir = core.config.lock().await.data_dir.clone();
    let path = data_dir.join("voice-state.json");
    let listening = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("listening").and_then(|b| b.as_bool()))
        .unwrap_or(false);
    if let Ok(s) = serde_json::to_string_pretty(
        &json!({ "listening": listening, "session_id": req.session_id }),
    ) {
        let _ = std::fs::write(&path, s);
    }
    Json(json!({"ok": true}))
}

async fn voice_beep(State(core): State<SharedState>) -> Json<Value> {
    let cfg = core.config.lock().await;
    let beep = cfg.beep_file.clone();
    drop(cfg);
    let ok = core.voice.beep(Some(&beep)).await.is_ok();
    Json(json!({"ok": ok}))
}

async fn voice_interrupt(State(core): State<SharedState>) -> Json<Value> {
    let ok = core.voice.interrupt().await.is_ok();
    Json(json!({"ok": ok}))
}

/// 连续对话（多轮讨论）模式开关。enabled=true 需监听已开启；false 立即退出。
async fn voice_continuous(
    State(core): State<SharedState>,
    Json(req): Json<ListeningReq>,
) -> Json<Value> {
    if req.enabled {
        if !core.config.lock().await.voice_enabled {
            return Json(json!({"ok": false, "error": "语音功能未启用"}));
        }
        // 连续对话建立在"监听已开启"之上：以 voice-serve 的真实状态为准
        let listening = core
            .voice
            .status()
            .await
            .ok()
            .and_then(|v| v.get("listening").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if !listening {
            return Json(json!({"ok": false, "error": "请先开启语音监听"}));
        }
        crate::agents::start_continuous(core.clone()).await;
    } else {
        core.continuous_active.store(false, Ordering::SeqCst);
    }
    Json(json!({"ok": true, "continuous_active": core.continuous_active.load(Ordering::SeqCst)}))
}

// ---------- 配置 ----------

async fn get_config(State(core): State<SharedState>) -> Json<Value> {
    // 以配置文件为唯一真相源：外部直接改动 config.json 后，
    // UI 刷新即可看到新值，避免内存缓存与文件不一致。
    // 文件不可读/损坏时回退内存当前值。
    let path = core.config_path.clone();
    match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<crate::config::Config>(&s).ok())
    {
        Some(cfg) => Json(serde_json::to_value(&cfg).unwrap_or_default()),
        None => {
            let cfg = core.config.lock().await;
            Json(serde_json::to_value(&*cfg).unwrap_or_default())
        }
    }
}

// OpenCode 后端模型清单接口已移除（仅保留内置 API 后端）。

async fn save_config(
    State(core): State<SharedState>,
    Json(cfg): Json<crate::config::Config>,
) -> Json<Value> {
    let mut guard = core.config.lock().await;
    // 数据目录不随配置保存改变（路径来自启动参数）
    let data_dir = guard.data_dir.clone();
    let mut cfg = cfg;
    cfg.data_dir = data_dir;
    if let Err(e) = cfg.save(&core.config_path) {
        return Json(json!({"ok": false, "error": format!("保存失败: {e}")}));
    }
    *guard = cfg.clone();
    let kws_threshold = cfg.kws_threshold;
    drop(guard);
    // 同步语音客户端
    core.voice.set_base(cfg.voice_base());
    // 热更新唤醒词阈值（voice-serve 未启动/失败时忽略，不影响配置保存）
    let vc = core.voice.clone();
    tokio::spawn(async move {
        if let Err(e) = vc.set_kws_threshold(kws_threshold).await {
            tracing::warn!("推送 KWS 阈值失败（voice-serve 可能未运行）: {e}");
        }
    });
    // 重建双 Agent
    let core2 = core.clone();
    tokio::spawn(async move {
        rebuild_agents(&core2).await;
        core2.emit(Event::SettingsUpdated);
    });
    Json(json!({"ok": true}))
}

// ---------- Skills ----------

async fn list_skills(State(core): State<SharedState>) -> Json<Value> {
    let list = core.skills.list().await;
    Json(serde_json::to_value(&list).unwrap_or_default())
}

#[derive(Deserialize)]
struct ImportSkillReq {
    path: String,
}

async fn import_skill(
    State(core): State<SharedState>,
    Json(req): Json<ImportSkillReq>,
) -> Json<Value> {
    match core.skills.import(&req.path).await {
        Ok(s) => Json(json!({"ok": true, "skill": s})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn remove_skill(State(core): State<SharedState>, Path(name): Path<String>) -> Json<Value> {
    match core.skills.remove(&name).await {
        Ok(_) => {
            core.emit(Event::SkillsUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct ToggleSkillReq {
    enabled: bool,
}

async fn toggle_skill(
    State(core): State<SharedState>,
    Path(name): Path<String>,
    Json(req): Json<ToggleSkillReq>,
) -> Json<Value> {
    match core.skills.set_enabled(&name, req.enabled).await {
        Ok(_) => {
            core.emit(Event::SkillsUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---------- MCP 连接器 ----------

async fn list_mcp(State(core): State<SharedState>) -> Json<Value> {
    let servers = core.mcp.list().await;
    // 只探测已启用的服务；停用的直接给空状态（不启动其子进程）
    let probes = futures::future::join_all(servers.iter().map(|s| async {
        if s.enabled {
            core.mcp.probe(s).await
        } else {
            crate::mcp::McpStatus::default()
        }
    }));
    let statuses = probes.await;
    let items: Vec<Value> = servers
        .iter()
        .zip(statuses.into_iter())
        .map(|(s, st)| {
            json!({
                "name": s.name,
                "transport": s.transport,
                "command": s.command,
                "args": s.args,
                "url": s.url,
                "enabled": s.enabled,
                "status": st,
            })
        })
        .collect();
    Json(json!({"ok": true, "servers": items}))
}

async fn add_mcp(
    State(core): State<SharedState>,
    Json(cfg): Json<crate::mcp::McpServerConfig>,
) -> Json<Value> {
    if let Err(e) = core.mcp.upsert(cfg.clone()).await {
        return Json(json!({"ok": false, "error": e}));
    }
    // 停用的服务不探测：否则添加配置时也会拉起它的子进程
    let status = if cfg.enabled {
        core.mcp.probe(&cfg).await
    } else {
        crate::mcp::McpStatus::default()
    };
    Json(json!({"ok": true, "name": cfg.name, "status": status}))
}

async fn remove_mcp(State(core): State<SharedState>, Path(name): Path<String>) -> Json<Value> {
    let removed = core.mcp.remove(&name).await;
    Json(json!({"ok": removed}))
}

async fn test_mcp(State(core): State<SharedState>, Path(name): Path<String>) -> Json<Value> {
    let Some(cfg) = core.mcp.list().await.into_iter().find(|s| s.name == name) else {
        return Json(json!({"ok": false, "error": "未找到该 MCP 服务"}));
    };
    let status = core.mcp.probe(&cfg).await;
    Json(json!({"ok": true, "status": status}))
}

// ---------- 记忆 ----------

async fn list_memory(State(core): State<SharedState>) -> Json<Value> {
    let list = core.memory.list().await;
    Json(serde_json::to_value(&list).unwrap_or_default())
}

#[derive(Deserialize)]
struct AddMemoryReq {
    #[serde(rename = "type")]
    memory_type: String,
    title: String,
    content: String,
}

async fn add_memory(State(core): State<SharedState>, Json(req): Json<AddMemoryReq>) -> Json<Value> {
    match core
        .memory
        .add(&req.memory_type, &req.title, &req.content)
        .await
    {
        Ok(e) => {
            core.emit(Event::MemoryUpdated);
            Json(json!({"ok": true, "entry": e}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn remove_memory(State(core): State<SharedState>, Path(id): Path<String>) -> Json<Value> {
    match core.memory.remove(&id).await {
        Ok(_) => {
            core.emit(Event::MemoryUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct UpdateMemoryReq {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

async fn update_memory(
    State(core): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMemoryReq>,
) -> Json<Value> {
    match core
        .memory
        .update(&id, req.title.as_deref(), req.content.as_deref())
        .await
    {
        Ok(true) => {
            core.emit(Event::MemoryUpdated);
            Json(json!({"ok": true}))
        }
        Ok(false) => Json(json!({"ok": false, "error": "记忆条目不存在"})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn clear_memory(State(core): State<SharedState>) -> Json<Value> {
    match core.memory.clear().await {
        Ok(_) => {
            core.emit(Event::MemoryUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---------- 定时任务 ----------

async fn list_schedules(State(core): State<SharedState>) -> Json<Value> {
    let list = core.scheduler.list().await;
    Json(serde_json::to_value(&list).unwrap_or_default())
}

#[derive(Deserialize)]
struct AddScheduleReq {
    title: String,
    prompt: String,
    cron_expr: String,
}

async fn add_schedule(
    State(core): State<SharedState>,
    Json(req): Json<AddScheduleReq>,
) -> Json<Value> {
    match core
        .scheduler
        .add(&req.title, &req.prompt, &req.cron_expr)
        .await
    {
        Ok(t) => {
            core.emit(Event::SchedulesUpdated);
            Json(json!({"ok": true, "task": t}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize, Default)]
struct UpdateScheduleReq {
    title: Option<String>,
    prompt: Option<String>,
    cron_expr: Option<String>,
    enabled: Option<bool>,
}

async fn update_schedule(
    State(core): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateScheduleReq>,
) -> Json<Value> {
    match core
        .scheduler
        .update(
            &id,
            req.title.as_deref(),
            req.prompt.as_deref(),
            req.cron_expr.as_deref(),
            req.enabled,
        )
        .await
    {
        Ok(true) => {
            core.emit(Event::SchedulesUpdated);
            Json(json!({"ok": true}))
        }
        Ok(false) => Json(json!({"ok": false, "error": "任务不存在"})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn remove_schedule(State(core): State<SharedState>, Path(id): Path<String>) -> Json<Value> {
    match core.scheduler.remove(&id).await {
        Ok(_) => {
            core.emit(Event::SchedulesUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---------- 管理 ----------

async fn admin_rebuild(State(core): State<SharedState>) -> Json<Value> {
    let core2 = core.clone();
    tokio::spawn(async move {
        rebuild_agents(&core2).await;
        core2.emit(Event::SettingsUpdated);
    });
    Json(json!({"ok": true}))
}

/// 在系统文件管理器中定位文件（macOS：Finder 显示；Windows：资源管理器选中；Linux：打开所在目录）
#[derive(Deserialize)]
struct RevealReq {
    path: String,
}

async fn reveal_path(Json(req): Json<RevealReq>) -> Json<Value> {
    let p = req.path.trim().to_string();
    if p.is_empty() {
        return Json(json!({"ok": false, "error": "路径为空"}));
    }
    let path = std::path::PathBuf::from(&p);
    if !path.exists() {
        return Json(json!({"ok": false, "error": "文件不存在"}));
    }
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .status()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&path)
            .status()
    } else {
        let dir = path.parent().unwrap_or(&path);
        std::process::Command::new("xdg-open").arg(dir).status()
    };
    let ok = matches!(status, Ok(s) if s.success());
    Json(json!({"ok": ok}))
}

/// 打开 macOS「隐私与安全性 → 麦克风」设置面板（引导用户授权）
async fn open_mic_settings() -> Json<Value> {
    if cfg!(target_os = "macos") {
        let r = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
            .status();
        let ok = matches!(r, Ok(s) if s.success());
        return Json(json!({"ok": ok}));
    }
    Json(json!({"ok": false, "error": "仅 macOS 支持"}))
}
