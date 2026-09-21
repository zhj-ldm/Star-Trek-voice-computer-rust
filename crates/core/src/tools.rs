//! 主 Agent 自定义工具：语音播报、派发子 Agent、打断、监控、导入 skill。
//! 这些工具是主 Agent 能力的全部来源（配合 WebSearch）。

use crate::events::Event;
use crate::state::{CoreState, TaskInfo};
use async_trait::async_trait;
use open_agent_sdk::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn str_of(input: &Value, key: &str, default: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

// ============================================================
// SpeakToUser —— 主 Agent 播报语音（必须至少调用一次）
// ============================================================

pub struct SpeakToUser {
    core: Arc<CoreState>,
}

impl SpeakToUser {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for SpeakToUser {
    fn name(&self) -> &str {
        "SpeakToUser"
    }
    fn description(&self) -> &str {
        "Speak a sentence aloud to the user via TTS. Hard rule: you MUST call this tool at least once before ending this turn — never finish a reply without having spoken at least once, no matter how short the reply is. Normally once per turn is enough: announce the final conclusion in one complete call. Do NOT call it at the very beginning of your turn (content would be incomplete). If the call returns a rejection saying speech is already playing, this turn has already been announced — do NOT retry, just finish your reply."
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "text".to_string(),
                json!({"type": "string", "description": "要播报的文本，一次尽量完整包含该轮要说的核心内容，避免分多条播报"}),
            )]),
            required: vec!["text".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let text = str_of(&input, "text", "");
        if text.is_empty() {
            return Ok(ToolResult::error("没有可播报的文本"));
        }
        // 互斥：已有语音正在播报时拒绝本次调用（不排队、不累积）。
        // 避免工具循环里连续多次 SpeakToUser 并发起多个子进程播放导致声音重叠；
        // 同时明确告知 agent 本轮已播报过，直接继续回答，不要重试。
        if self.core.speaking.load(Ordering::SeqCst) {
            return Ok(ToolResult::error(
                "Speech is already playing to the user; this turn has already been announced. Do NOT call SpeakToUser again — just finish your reply.",
            ));
        }
        self.core.emit(Event::Voice {
            kind: "speak_start".into(),
            text: text.clone(),
        });
        self.core.speaking.store(true, Ordering::SeqCst);
        // 标记本轮已播报（run_main_turn 结束检查用）
        self.core.turn_spoken.store(true, Ordering::SeqCst);

        // TTS 后台异步播报，不阻塞 agent 回复循环。
        // 原实现阻塞式合成+播放（最长 120s 超时），会让 run_loop 卡在工具执行上：
        // main_busy 长期占用 → 前端"正在处理…"一直转、后续提问直接被拒。
        let core = self.core.clone();
        let text2 = text.clone();
        tokio::spawn(async move {
            let cfg = {
                let c = core.config.lock().await;
                (
                    c.tts_backend.clone(),
                    c.voice.clone(),
                    c.rate,
                    c.goose_tts_path.clone(),
                )
            };
            let result: Result<(), String> = match cfg.0.as_str() {
                "goose-tts" => {
                    let (bin, text, voice, rate) = (
                        cfg.3.clone(),
                        text2.clone(),
                        cfg.1.clone(),
                        cfg.2,
                    );
                    let vc = core.voice.clone();
                    match tokio::task::spawn_blocking(move || {
                        vc.speak_goose_tts(&bin, &text, &voice, rate)
                    })
                    .await
                    {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                }
                _ => core
                    .voice
                    .speak(&text2, &cfg.1, cfg.2)
                    .await
                    .map_err(|e| e.to_string()),
            };
            core.speaking.store(false, Ordering::SeqCst);
            core.emit(Event::Voice {
                kind: "speak_end".into(),
                text: text2,
            });
            if let Err(e) = result {
                // 播报失败：复位本轮播报标记，让 run_main_turn 的结束检查
                // 有机会触发"强制重播"路径，避免出现整轮无语音播报。
                core.turn_spoken.store(false, Ordering::SeqCst);
                tracing::warn!("TTS 播报失败: {e}");
            }
        });

        Ok(ToolResult::text("已开始向用户播报"))
    }
}

// ============================================================
// DispatchTask —— 派发子 Agent 任务（不阻塞主 Agent）
// ============================================================

pub struct DispatchTask {
    core: Arc<CoreState>,
}

impl DispatchTask {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for DispatchTask {
    fn name(&self) -> &str {
        "DispatchTask"
    }
    fn description(&self) -> &str {
        "把需要完整能力的复杂任务派发给子 Agent 执行（文件操作、代码、网页、深度调研等）。\
调用后立即返回任务 ID，主 Agent 可继续与用户交互；子 Agent 完成后会自动汇报。\
需要时可用 MonitorSubagent 查询进度、StopSubagent 打断。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "name".to_string(),
                    json!({"type": "string", "description": "任务名称（简短）"}),
                ),
                (
                    "instruction".to_string(),
                    json!({"type": "string", "description": "给子 Agent 的完整任务指令"}),
                ),
            ]),
            required: vec!["instruction".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let name = str_of(&input, "name", "子任务");
        let instruction = str_of(&input, "instruction", "");
        if instruction.is_empty() {
            return Ok(ToolResult::error("缺少任务指令 instruction"));
        }
        // 单并发约束：同一时刻只允许一个子 Agent 任务在运行。
        // 双保险：sub_busy 原子标志 + 任务表内 running 状态（防止 StopSubagent
        // 已改状态但旧任务尚未退出的窗口期并发）。
        {
            let busy = self.core.sub_busy.load(Ordering::SeqCst);
            let tasks = self.core.tasks.lock().await;
            let has_running = tasks.values().any(|t| t.status == "running");
            if busy || has_running {
                return Ok(ToolResult::error(
                    "已有子 Agent 任务正在运行（同一时刻只允许一个子任务并发）。\
                     请勿重复派发新任务，可等待当前任务自动汇报完成后再派发，\
                     或用 MonitorSubagent 查询当前进度、StopSubagent 打断当前任务。",
                ));
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        let info = TaskInfo {
            id: id.clone(),
            name: name.clone(),
            instruction: instruction.clone(),
            status: "running".into(),
            created_at: chrono::Local::now().to_rfc3339(),
            summary: String::new(),
            report_ready: false,
        };
        {
            let mut tasks = self.core.tasks.lock().await;
            tasks.insert(id.clone(), info);
        }
        self.core.emit(Event::SubagentStart {
            task_id: id.clone(),
            name: name.clone(),
        });
        // 后台执行子 Agent（不阻塞主 Agent 的当前轮）
        let core = self.core.clone();
        let tid = id.clone();
        tokio::spawn(async move {
            crate::agents::run_subtask(core, tid).await;
        });
        Ok(ToolResult::text(format!(
            "已派发任务，任务ID: {id}。子 Agent 正在后台执行，完成后会自动语音汇报。\
你可以先向用户说明任务已派发。"
        )))
    }
}

// ============================================================
// MonitorSubagent —— 查询子 Agent 任务进度
// ============================================================

pub struct MonitorSubagent {
    core: Arc<CoreState>,
}

impl MonitorSubagent {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for MonitorSubagent {
    fn name(&self) -> &str {
        "MonitorSubagent"
    }
    fn description(&self) -> &str {
        "查询子 Agent 任务状态与进度。输入任务 ID 查询单个任务；不传 task_id 时返回当前所有子 Agent 任务（含 running/done/error）的状态列表。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "task_id".to_string(),
                json!({"type": "string", "description": "要查询的任务 ID，可省略（省略时列出所有任务）"}),
            )]),
            required: vec![],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let tid = str_of(&input, "task_id", "");
        let tasks = self.core.tasks.lock().await;
        if tid.is_empty() {
            // 不传 ID：列出全部任务，模型据此找到活跃任务，避免"派发后查不到进度"
            if tasks.is_empty() {
                return Ok(ToolResult::text("当前没有任何子 Agent 任务记录。"));
            }
            let mut lines: Vec<String> = tasks
                .values()
                .map(|t| {
                    format!(
                        "- 任务[{}] 状态: {}；名称: {}；输出摘要: {}",
                        t.id, t.status, t.name, t.summary
                    )
                })
                .collect();
            lines.sort();
            let list = lines.join("\n");
            let running = tasks.values().filter(|t| t.status == "running").count();
            Ok(ToolResult::text(format!(
                "当前共有 {} 个子 Agent 任务，其中 {} 个运行中：\n{}",
                tasks.len(),
                running,
                list
            )))
        } else {
            match tasks.get(&tid) {
                Some(t) => Ok(ToolResult::text(format!(
                    "任务[{}] 状态: {}；名称: {}；输出摘要: {}",
                    t.id, t.status, t.name, t.summary
                ))),
                None => Ok(ToolResult::text(format!("未找到任务 {tid}"))),
            }
        }
    }
}

// ============================================================
// StopSubagent —— 打断子 Agent 任务
// ============================================================

pub struct StopSubagent {
    core: Arc<CoreState>,
}

impl StopSubagent {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for StopSubagent {
    fn name(&self) -> &str {
        "StopSubagent"
    }
    fn description(&self) -> &str {
        "打断正在执行的子 Agent 任务。输入任务 ID；不传则打断所有子任务。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "task_id".to_string(),
                json!({"type": "string", "description": "要打断的任务 ID，可省略"}),
            )]),
            required: vec![],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        self.core.interrupt_sub.store(true, Ordering::SeqCst);
        let tid = str_of(&input, "task_id", "");
        if !tid.is_empty() {
            let mut tasks = self.core.tasks.lock().await;
            if let Some(t) = tasks.get_mut(&tid) {
                if t.status == "running" {
                    t.status = "interrupted".into();
                }
            }
            self.core.emit(Event::SubagentError {
                task_id: tid.clone(),
                message: "已被主 Agent 打断".into(),
            });
        }
        Ok(ToolResult::text("已请求打断子 Agent 任务"))
    }
}

// ============================================================
// ImportSkill —— 让 AI 导入 skill 文件夹
// ============================================================

pub struct ImportSkill {
    core: Arc<CoreState>,
}

impl ImportSkill {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for ImportSkill {
    fn name(&self) -> &str {
        "ImportSkill"
    }
    fn description(&self) -> &str {
        "导入一个 skill 文件夹到 skills 系统。skill 文件夹需包含运行文件与说明文档（如 README.md）。\
输入文件夹绝对路径即可。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "path".to_string(),
                json!({"type": "string", "description": "skill 文件夹的绝对路径"}),
            )]),
            required: vec!["path".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let path = str_of(&input, "path", "");
        if path.is_empty() {
            return Ok(ToolResult::error("缺少路径 path"));
        }
        match self.core.skills.import(&path).await {
            Ok(skill) => {
                self.core.emit(Event::SkillsUpdated);
                Ok(ToolResult::text(format!(
                    "已导入 skill「{}」：{}",
                    skill.name, skill.description
                )))
            }
            Err(e) => Ok(ToolResult::error(format!("导入失败: {e}"))),
        }
    }
}
