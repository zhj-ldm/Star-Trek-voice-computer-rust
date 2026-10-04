//! 事件类型：core → UI (SSE) 与 core 内部广播共用。
//! UI 与后端必须一一对应，新增事件需同步前端。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// agent 工作状态
    AgentStatus {
        agent: String, // "main" | "sub"
        status: String, // "idle" | "working" | "speaking"
    },
    /// 用户文本进入主 agent（session_id 为空 = 全局，前端按当前会话渲染）
    UserText { session_id: String, text: String },
    /// 子 agent 汇报文本进入主 agent（独立事件，不以用户消息形式出现）
    ReportText { session_id: String, text: String },
    /// 主 agent 流式文本
    AssistantText { session_id: String, text: String },
    /// 主 agent 一轮完成（最终文本 + 本轮墙钟耗时 ms）
    AssistantDone {
        session_id: String,
        text: String,
        #[serde(default)]
        elapsed_ms: u64,
    },
    /// 工具调用开始/结束
    ToolUse {
        session_id: String,
        agent: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        session_id: String,
        agent: String,
        name: String,
        ok: bool,
        summary: String,
    },
    /// AI 中间思考过程（主 agent，前端以左箭头折叠卡展示）
    ReasoningText { session_id: String, text: String },
    /// 子 agent 生命周期
    SubagentStart { task_id: String, name: String },
    SubagentProgress { task_id: String, message: String },
    SubagentDone { task_id: String, summary: String },
    SubagentError { task_id: String, message: String },
    /// 子 agent 工具调用（任务面板实时展示，不进对话流）
    SubagentToolUse {
        task_id: String,
        name: String,
        input: serde_json::Value,
    },
    SubagentToolResult {
        task_id: String,
        name: String,
        ok: bool,
        summary: String,
    },
    /// 语音链路事件
    Voice {
        kind: String, // "wakeword" | "stt" | "speak_start" | "speak_end"
        text: String,
    },
    /// 定时任务触发
    ScheduleTriggered { id: String, title: String },
    /// 数据变更（前端刷新对应列表）
    MemoryUpdated,
    SkillsUpdated,
    SettingsUpdated,
    SchedulesUpdated,
    /// 子 agent 汇报完成（主 agent 需向用户播报）
    SubagentReportReady { task_id: String, text: String },
}
