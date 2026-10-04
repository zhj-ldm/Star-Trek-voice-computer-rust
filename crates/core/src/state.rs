//! 共享状态：配置、双 Agent、事件广播、打断标志、语音客户端。

use crate::agents::{MainAgent, SubAgent};
use crate::config::Config;
use crate::events::Event;
use crate::memory::MemoryStore;
use crate::scheduler::Scheduler;
use crate::sessions::SessionStore;
use crate::skills::SkillManager;
use crate::voice::VoiceClient;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

pub struct CoreState {
    pub config: Arc<Mutex<Config>>,
    pub config_path: std::path::PathBuf,
    pub events: broadcast::Sender<Event>,
    /// 主 agent 会话（&mut 由 Mutex 保护）
    pub main_agent: Arc<Mutex<Option<MainAgent>>>,
    /// 子 agent 会话
    pub sub_agent: Arc<Mutex<Option<SubAgent>>>,
    /// 语音链路客户端
    pub voice: VoiceClient,
    pub skills: SkillManager,
    /// 本地 Ollama 模型名缓存（用于 -nothink 后缀自动匹配，重建 Agent 时刷新）
    pub ollama_models_cache: Arc<tokio::sync::Mutex<Vec<String>>>,
    pub memory: MemoryStore,
    pub scheduler: Scheduler,
    /// 多会话存储（对话历史持久化）
    pub sessions: Arc<tokio::sync::Mutex<SessionStore>>,
    /// 子 agent 任务登记表（DispatchTask 创建，自动汇报用）
    pub tasks: Arc<Mutex<std::collections::HashMap<String, TaskInfo>>>,
    /// 主 agent 是否正在工作
    pub main_busy: Arc<AtomicBool>,
    /// 子 agent 是否正在工作
    pub sub_busy: Arc<AtomicBool>,
    /// 打断主 agent 信号（语音 stop 触发）
    pub interrupt_main: Arc<AtomicBool>,
    /// 打断子 agent 信号
    pub interrupt_sub: Arc<AtomicBool>,
    /// 正在 TTS 播报
    pub speaking: Arc<AtomicBool>,
    /// 语音唤醒处理中（防 KWS 重复回调导致同一句话双发）
    pub voice_active: Arc<AtomicBool>,
    /// 是否已初始化 agent（API 配置就绪后）
    pub agents_ready: Arc<AtomicBool>,
    /// 语音对话目标会话（前端开启监听/切换会话时同步；None = 跟随 active）
    pub voice_session: Arc<Mutex<Option<String>>>,
    /// 当前主任务所属会话（权威来源；前端切换会话时据此恢复进行态/按钮）
    pub turn_session: Arc<Mutex<Option<String>>>,
}

/// 子 agent 任务信息
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskInfo {
    pub id: String,
    pub name: String,
    pub instruction: String,
    pub status: String, // "running" | "done" | "error" | "interrupted"
    pub created_at: String,
    pub summary: String,
    pub report_ready: bool,
}

impl CoreState {
    pub fn new(config: Config, config_path: std::path::PathBuf) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            config: Arc::new(Mutex::new(config)),
            config_path,
            events: tx,
            main_agent: Arc::new(Mutex::new(None)),
            sub_agent: Arc::new(Mutex::new(None)),
            voice: VoiceClient::default(),
            skills: SkillManager::default(),
            ollama_models_cache: Arc::new(Mutex::new(Vec::new())),
            memory: MemoryStore::default(),
            scheduler: Scheduler::default(),
            sessions: Arc::new(Mutex::new(SessionStore::new())),
            tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            main_busy: Arc::new(AtomicBool::new(false)),
            sub_busy: Arc::new(AtomicBool::new(false)),
            interrupt_main: Arc::new(AtomicBool::new(false)),
            interrupt_sub: Arc::new(AtomicBool::new(false)),
            speaking: Arc::new(AtomicBool::new(false)),
            voice_active: Arc::new(AtomicBool::new(false)),
            agents_ready: Arc::new(AtomicBool::new(false)),
            voice_session: Arc::new(Mutex::new(None)),
            turn_session: Arc::new(Mutex::new(None)),
        }
    }

    pub fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    pub fn set_main_status(&self, status: &str) {
        self.emit(Event::AgentStatus {
            agent: "main".into(),
            status: status.into(),
        });
    }

    pub fn set_sub_status(&self, status: &str) {
        self.emit(Event::AgentStatus {
            agent: "sub".into(),
            status: status.into(),
        });
    }

    pub fn is_main_interrupted(&self) -> bool {
        self.interrupt_main.load(Ordering::SeqCst)
    }
}
