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
        /// 本轮是否被打断（前端据此显示「被打断」而不是最终回答）
        #[serde(default)]
        interrupted: bool,
    },
    /// 工具调用开始/结束（id = 上游 tool_call_id，前端按 id 精确配对卡片）
    ToolUse {
        session_id: String,
        agent: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        session_id: String,
        agent: String,
        id: String,
        name: String,
        ok: bool,
        summary: String,
    },
    /// 本轮模型请求失败（前端渲染错误卡片 + 重试入口）
    TurnFailed {
        session_id: String,
        message: String,
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
    /// 子 Agent 派发 / 进度 / 完成
    SubagentStarted {
        id: String,
        session_id: String,
        session_title: String,
        title: String,
        prompt: String,
    },
    SubagentProgress {
        id: String,
        session_id: String,
        progress: String,
    },
    SubagentDone {
        id: String,
        session_id: String,
        session_title: String,
        title: String,
        summary: String,
        ok: bool,
        status: String,
    },
    /// 子 Agent 执行轨迹（逐条推送，详情页实时展示）
    SubagentTrace {
        id: String,
        session_id: String,
        entry: serde_json::Value,
    },
    /// 子 Agent 任务记录被删除（含批量清理）
    SubagentDeleted {
        id: String,
        session_id: String,
        with_conversation: bool,
    },
    /// MCP 连接器列表变更（AI 自主连接/断开后刷新前端列表）
    McpUpdated,
    SettingsUpdated,
    SchedulesUpdated,
    /// SSE 断流期间事件缺口过大、环形缓冲也无法补齐：前端需整体刷新一次
    Resync,
}
