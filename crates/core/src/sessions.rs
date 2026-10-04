//! 多会话（对话历史）存储：每个会话独立保存 user/assistant 消息，
//! 持久化到 data/sessions.json。切换会话时恢复 Agent 上下文。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 单条工具调用记录（随 assistant 消息持久化，供切换会话后恢复渲染）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: serde_json::Value,
    pub ok: bool,
    pub summary: String,
    /// 是否已收到工具结果（false = 仍运行中，被打断时补发"已中断"状态）
    #[serde(default)]
    pub done: bool,
}

/// 单条对话消息
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMsg {
    pub role: String, // "user" | "assistant"
    pub text: String,
    pub created_at: String,
    /// 该 assistant 消息的工具调用记录（仅 assistant 消息填充）
    #[serde(default)]
    pub tools: Vec<ToolCallRecord>,
}

/// 一个会话
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<ChatMsg>,
}

impl Session {
    fn new(title: Option<&str>) -> Self {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            id,
            title: title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "新对话".into()),
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoreFile {
    active_id: Option<String>,
    sessions: Vec<Session>,
}

/// 会话仓库（tokio Mutex 由调用方持有）
#[derive(Default)]
pub struct SessionStore {
    path: Option<PathBuf>,
    data: StoreFile,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init(&mut self, path: PathBuf) {
        self.path = Some(path.clone());
        if let Ok(s) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<StoreFile>(&s) {
                Ok(d) => self.data = d,
                Err(e) => tracing::warn!("解析 sessions.json 失败，使用空会话库: {e}"),
            }
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.data) {
            let _ = std::fs::write(path, s);
        }
    }

    // ---------- 查询 ----------
    pub fn list(&self) -> Vec<Session> {
        let mut v = self.data.sessions.clone();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v
    }

    pub fn get(&self, id: &str) -> Option<Session> {
        self.data.sessions.iter().find(|s| s.id == id).cloned()
    }

    pub fn active_id(&self) -> Option<String> {
        self.data
            .active_id
            .clone()
            .filter(|id| self.data.sessions.iter().any(|s| &s.id == id))
    }

    pub fn active(&self) -> Option<Session> {
        self.active_id().and_then(|id| self.get(&id))
    }

    /// 确保存在一个有效会话；无则创建并设为 active
    pub fn ensure_active(&mut self) -> Session {
        if let Some(s) = self.active() {
            return s;
        }
        let s = Session::new(Some("新对话"));
        let id = s.id.clone();
        self.data.sessions.push(s);
        self.data.active_id = Some(id);
        self.save();
        self.active().expect("just created")
    }

    // ---------- 变更 ----------
    pub fn create(&mut self, title: Option<&str>) -> Session {
        let s = Session::new(title);
        let id = s.id.clone();
        self.data.sessions.push(s);
        self.data.active_id = Some(id.clone());
        self.save();
        self.get(&id).expect("just pushed")
    }

    pub fn switch(&mut self, id: &str) -> bool {
        if self.data.sessions.iter().any(|s| s.id == id) {
            self.data.active_id = Some(id.to_string());
            self.save();
            true
        } else {
            false
        }
    }

    pub fn rename(&mut self, id: &str, title: &str) -> bool {
        let t = title.trim();
        if t.is_empty() {
            return false;
        }
        if let Some(s) = self.data.sessions.iter_mut().find(|s| s.id == id) {
            s.title = t.to_string();
            s.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            self.save();
            true
        } else {
            false
        }
    }

    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.data.sessions.len();
        self.data.sessions.retain(|s| s.id != id);
        let removed = self.data.sessions.len() != before;
        if removed && self.data.active_id.as_deref() == Some(id) {
            self.data.active_id = None;
        }
        if removed {
            self.save();
        }
        removed
    }

    pub fn clear_messages(&mut self, id: &str) -> bool {
        if let Some(s) = self.data.sessions.iter_mut().find(|s| s.id == id) {
            s.messages.clear();
            s.updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            self.save();
            true
        } else {
            false
        }
    }

    /// 追加一条消息（若为第一条 user 消息且标题还是默认，用其更新标题）
    /// tools 仅对 assistant 消息生效；user 消息传入空 vec
    pub fn append(&mut self, id: &str, role: &str, text: &str, tools: Vec<ToolCallRecord>) {
        let Some(s) = self.data.sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        s.messages.push(ChatMsg {
            role: role.into(),
            text: text.into(),
            created_at: now.clone(),
            tools: if role == "assistant" { tools } else { Vec::new() },
        });
        s.updated_at = now;
        if role == "user" && s.title == "新对话" && s.messages.len() == 1 {
            let t = text.trim();
            if !t.is_empty() {
                s.title = t.chars().take(20).collect();
            }
        }
        self.save();
    }

    /// 取最近 N 条消息（用于恢复 Agent 上下文）
    pub fn recent_messages(&self, id: &str, n: usize) -> Vec<ChatMsg> {
        self.get(id)
            .map(|s| {
                let len = s.messages.len();
                s.messages.into_iter().skip(len.saturating_sub(n)).collect()
            })
            .unwrap_or_default()
    }
}
