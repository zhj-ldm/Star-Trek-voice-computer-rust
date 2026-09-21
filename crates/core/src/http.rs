//! HTTP API（axum）：core 与 Electron UI / voice-serve 的通信入口。
//! UI 与后端接口一一对应，新增接口需同步前端。

use crate::agents::{handle_wakeword, rebuild_agents, run_main_turn};
use crate::events::Event;
use crate::state::CoreState;
use axum::{
    extract::{Path, Request, State},
    middleware::{self, Next},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::{delete, get, post},
    Json, Router,
};
use axum::http::{HeaderValue, Method, StatusCode};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::convert::Infallible;

pub type SharedState = Arc<CoreState>;

/// 允许 Electron 渲染进程（任意 origin）直连本地 API。
async fn cors_layer(req: Request, next: Next) -> axum::response::Response {
    if req.method() == Method::OPTIONS {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        *resp.status_mut() = StatusCode::NO_CONTENT;
        set_cors(resp.headers_mut());
        return resp;
    }
    let mut resp = next.run(req).await;
    set_cors(resp.headers_mut());
    resp
}

fn set_cors(h: &mut axum::http::HeaderMap) {
    h.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
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
        .route("/api/voice/interrupt", post(voice_interrupt))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/skills", get(list_skills))
        .route("/api/skills/import", post(import_skill))
        .route("/api/skills/:name", delete(remove_skill))
        .route("/api/skills/:name/toggle", post(toggle_skill))
        .route("/api/memory", get(list_memory).post(add_memory))
        .route("/api/memory/:id", delete(remove_memory))
        .route("/api/memory/clear", post(clear_memory))
        .route("/api/schedules", get(list_schedules).post(add_schedule))
        .route("/api/schedules/:id", post(update_schedule).delete(remove_schedule))
        .route("/api/tasks", get(list_tasks))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/:id", delete(delete_session))
        .route("/api/sessions/:id/rename", post(rename_session))
        .route("/api/sessions/:id/switch", post(switch_session))
        .route("/api/sessions/:id/messages", get(get_session_messages))
        .route("/api/sessions/:id/clear", post(clear_session))
        .route("/api/admin/rebuild", post(admin_rebuild))
        .route("/api/system/open-mic-settings", post(open_mic_settings))
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
        "sub_status": if core.sub_busy.load(Ordering::SeqCst) {"working"} else {"idle"},
        "speaking": core.speaking.load(Ordering::SeqCst),
        "voice_active": core.voice_active.load(Ordering::SeqCst),
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

async fn events_sse(State(core): State<SharedState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = core.events.subscribe();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    yield Ok(SseEvent::default().data(data));
                }
                Err(_) => break,
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
    core.interrupt_main.store(true, Ordering::SeqCst);
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

async fn delete_session(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
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

async fn switch_session(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
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

async fn clear_session(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let mut sessions = core.sessions.lock().await;
    let ok = sessions.clear_messages(&id);
    Json(json!({"ok": ok}))
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
    *core.voice_session.lock().await = if req.enabled { req.session_id } else { None };
    let ok = core.voice.set_listening(req.enabled).await.is_ok();
    Json(json!({"ok": ok}))
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

// ---------- 配置 ----------

async fn get_config(State(core): State<SharedState>) -> Json<Value> {
    let cfg = core.config.lock().await;
    Json(serde_json::to_value(&*cfg).unwrap_or_default())
}

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

async fn remove_skill(
    State(core): State<SharedState>,
    Path(name): Path<String>,
) -> Json<Value> {
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

async fn add_memory(
    State(core): State<SharedState>,
    Json(req): Json<AddMemoryReq>,
) -> Json<Value> {
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

async fn remove_memory(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
    match core.memory.remove(&id).await {
        Ok(_) => {
            core.emit(Event::MemoryUpdated);
            Json(json!({"ok": true}))
        }
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

async fn remove_schedule(
    State(core): State<SharedState>,
    Path(id): Path<String>,
) -> Json<Value> {
    match core.scheduler.remove(&id).await {
        Ok(_) => {
            core.emit(Event::SchedulesUpdated);
            Json(json!({"ok": true}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---------- 子任务 ----------

async fn list_tasks(State(core): State<SharedState>) -> Json<Value> {
    let tasks = core.tasks.lock().await;
    let mut list: Vec<Value> = tasks
        .values()
        .map(|t| serde_json::to_value(t).unwrap_or_default())
        .collect();
    list.sort_by_key(|t| {
        t["created_at"]
            .as_str()
            .unwrap_or("")
            .to_string()
    });
    list.reverse();
    Json(Value::Array(list))
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
