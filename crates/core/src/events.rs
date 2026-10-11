//! 事件类型：core → UI (SSE) 与 core 内部广播共用。
//! UI 与后端必须一一对应，新增事件需同步前端。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// agent 工作状态
    AgentStatus {
        agent: String,  // "main"
        status: String, // "idle" | "working" | "speaking"
    },
    /// 用户文本进入主 agent（session_id 为空 = 全局，前端按当前会话渲染）
    UserText {
        session_id: String,
        text: String,
    },
    /// 汇报文本（独立事件，不以用户消息形式进入对话流）
    ReportText {
        session_id: String,
        text: String,
    },
    /// 主 agent 流式文本
    AssistantText {
        session_id: String,
        text: String,
    },
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
    ReasoningText {
        session_id: String,
        text: String,
    },
    /// 语音链路事件
    Voice {
        kind: String, // "wakeword" | "stt" | "speak_start" | "speak_end"
        text: String,
    },
    /// 定时任务触发
    ScheduleTriggered {
        id: String,
        title: String,
    },
    /// 数据变更（前端刷新对应列表）
    MemoryUpdated,
    SkillsUpdated,
    SettingsUpdated,
    SchedulesUpdated,
}
