---
AIGC:
    Label: "1"
    ContentProducer: 001191440300708461136T1XGW3
    ProduceID: abc280594deb8e830e487cdb027f0632_300b389baf1911f188ac525400dcc5b3
    ReservedCode1: rKQJXPfzCzasRHrpYiYQV1w4LCEW1Uyk/Cp+7BrYLDhUCc+hDRo/ELD8cJDUdsWzX2I9Eye4+f76bs2OSWpG6AKS/xXjLJWpH16sXu3WfEEHfpUXjOOXrPNBR8aOQE1FNRFORyz/ayUJjfoJ0CR2njWL4q3YgvQfGjBBr3N+Dx71tOMZ/ZNNCFYcozg=
    ContentPropagator: 001191440300708461136T1XGW3
    PropagateID: abc280594deb8e830e487cdb027f0632_300b389baf1911f188ac525400dcc5b3
    ReservedCode2: rKQJXPfzCzasRHrpYiYQV1w4LCEW1Uyk/Cp+7BrYLDhUCc+hDRo/ELD8cJDUdsWzX2I9Eye4+f76bs2OSWpG6AKS/xXjLJWpH16sXu3WfEEHfpUXjOOXrPNBR8aOQE1FNRFORyz/ayUJjfoJ0CR2njWL4q3YgvQfGjBBr3N+Dx71tOMZ/ZNNCFYcozg=
---

# Star Trek Computer · 星际迷航语音助手（Rust 版）

**版本 1.0.0** · Apache-2.0

基于 open-agent-sdk-rust 构建的完整星际迷航风格本地语音助手。Rust 后端 + Electron 纯白极简 UI，
纯 Rust 语音闭环（KWS 唤醒 / STT / TTS），支持双 Agent 编排、Skills、记忆、定时任务与内置联网搜索。

## 功能特性

- **语音交互**：喊 `computer` 唤醒，支持打断 / 停止关键词；TTS 播报（EdgeTTS internal / goose-tts 外部二进制）
- **双 Agent 架构**：主 Agent（精简工具：联网搜索 + 派发 / 监控 / 打断子 Agent + 语音播报 + 导入 Skill）
  + 子 Agent（完整工具能力，后台执行，完成后自动语音汇报）
- **内置搜索**：`WebSearchTool` 优先走内置 searchpin 引擎（resources/searchpin-ai，MCP stdio，零 API Key，
  四引擎并行 + 本地 embedding 重排），失败时回退自研多引擎搜索（Bing RSS → 百度 → 360）
- **Skills 系统**：文件夹即 skill，可 UI 导入或主 Agent 自动导入
- **记忆 / 定时任务 / 会话管理**：HTTP API + SSE 事件流全量支持
- **Electron UI**：纯白极简界面（对话 / 记忆 / Skills / 定时 / 子任务 / 设置）

## 架构总览

```
┌────────────────────────────────────────────────────────────┐
│ Electron UI（ui/）  纯白极简 · 对话/记忆/Skills/定时/子任务/设置 │
│   ├─ electron/   主进程：拉起 core 后端，退出时终止后端进程组   │
│   ├─ src/*       渲染层：fetch 直连本地 API + SSE 事件流       │
│   └─ package.json                                            │
└───────────────┬────────────────────────────────────────────┘
                │ HTTP (127.0.0.1:8410, CORS 放行)
┌───────────────▼────────────────────────────────────────────┐
│ star-trek-core（crates/core，Rust + axum，DEFAULT_CORE_PORT=8410）│
│   · 双 Agent 编排：主 Agent / 子 Agent（crates/agents = vendored SDK）│
│   · HTTP API：/api/*（状态/对话/语音/配置/记忆/Skills/定时/子任务）│
│   · SSE：/api/events 事件流                                   │
│   · 唤醒词回调入口 /api/voice/wakeword                        │
│   · 并行 spawn 语音进程 voice-serve（同进程组）                │
└───────────────┬────────────────────────────────────────────┘
                │ spawn + HTTP (127.0.0.1:8420, VOICE_PORT)
┌───────────────▼────────────────────────────────────────────┐
│ voice-serve（crates/voice，独立 Rust 进程）                    │
│   · KWS 唤醒词监听（sherpa-onnx，低频轮询 + 唤醒窗口）         │
│   · STT：Paraformer（funasr rust 系，替代 whisper）           │
│   · TTS：edge-tts-rust（internal）或外部 goose-tts 二进制       │
│   · 提示音播放（computer_beep_1.mp3）                         │
└────────────────────────────────────────────────────────────┘
```

## 目录结构

```
star-trek-assistant/
├─ Cargo.toml / Cargo.lock         workspace（license = Apache-2.0）
├─ crates/
│  ├─ agents/                      vendored open-agent-sdk-rust（开源框架，基本不动）
│  ├─ core/                        star-trek-core 主后端（双 Agent 编排 + HTTP API）
│  │  └─ src/
│  │     ├─ main.rs                 启动：config/skills/memory/scheduler + spawn voice-serve + HTTP
│  │     ├─ config.rs               双 Agent API / 语音阈值 / skills 目录 / TTS 后端
│  │     ├─ agents.rs               构建主/子 Agent、会话轮、唤醒词处理、子任务执行
│  │     ├─ tools.rs                主 Agent 自定义工具（SpeakToUser/DispatchTask/...）
│  │     ├─ search.rs               searchpin 子进程客户端 + 多引擎搜索 fallback
│  │     ├─ sessions.rs             会话管理
│  │     ├─ skills.rs / memory.rs / scheduler.rs / events.rs / state.rs / http.rs / voice.rs
│  └─ voice/                        voice-serve 语音进程（KWS/STT/TTS/提示音）
├─ ui/                              Electron 界面（package.json + electron/ + src/）
├─ resources/                       运行资源：音效、goose-tts、searchpin-ai 二进制
├─ data/                            运行时数据（不入库，需按 README 配置）
├─ run.sh                           一键启动脚本
└─ LICENSE                          Apache-2.0
```

## 快速开始

```bash
# 1. 克隆仓库（已包含 vendored open-agent-sdk-rust，clone 后即可编译）
git clone https://github.com/zhj-ldm/Star-Trek-Computer-Rust.git
cd Star-Trek-Computer-Rust

# 2. 编译后端（含 core + voice-serve）
cargo build --release

# 3. 安装 UI 依赖（含 Electron 二进制）
cd ui && npm install && cd ..

# 4. 配置 data/config.json（见下方「配置」）

# 5. 启动（自动拉起后端 + 语音进程）
./run.sh
```

> 终端运行 `./run.sh` 以继承 macOS 麦克风（TCC）授权。关闭窗口后 Electron 主进程会终止整个后端进程组。

### 编译前置说明

- voice-serve 依赖 `sherpa-onnx` 的 prebuilt 库。若本地未配置
  `SHERPA_ONNX_LIB_DIR` 环境变量，sherpa-onnx 构建脚本会自动下载对应平台 prebuilt；
  也可自行下载静态库并设置环境变量指向其 lib 目录后重新 `cargo build`。
- 建议使用稳定版 Rust toolchain（本项目在 Rust 1.97 下验证通过）。

## 配置

`data/` 目录不入库（内含 API Key 等敏感信息）。首次运行前创建 `data/config.json`：

```json
{
  "data_dir": "data",
  "main_base_url": "https://api.openai.com/v1",
  "main_api_key": "YOUR_API_KEY",
  "main_model": "gpt-4o",
  "main_system_prompt": "你是星际迷航风格电脑助手...",
  "sub_base_url": "https://api.openai.com/v1",
  "sub_api_key": "YOUR_API_KEY",
  "sub_model": "gpt-4o",
  "sub_system_prompt": "你是子 Agent...",
  "voice_host": "127.0.0.1",
  "voice_port": 8420,
  "wakeword": "computer",
  "beep_file": "resources/computer_beep_1.mp3",
  "voice": "en-US-AndrewMultilingualNeural",
  "rate": "+0%",
  "tts_backend": "internal",
  "goose_tts_path": "resources/goose-tts",
  "interrupt_keywords": ["stop", "停止", "停", "够了", "取消"],
  "max_record_secs": 15,
  "voice_enabled": true,
  "skill_dirs": [],
  "max_turns": 20
}
```

也可以在 UI「设置」页配置主 / 子 Agent 的 Base URL、API Key、模型与 System Prompt
（同一服务商可配两个不同 Key 或模型），以及语音阈值、TTS 后端等。

## 语音交互（双唤醒场景）

- 主 Agent 空闲：喊 `computer` → 提示音 → 录音转文字 → 作为指令发送给主 Agent。
- 主 Agent 干活中：喊 `computer` → 打断当前 TTS → 录音转文字；
  - 识别到 `stop / 停止 / 停 / 够了 / 取消`（可配置）→ 仅打断当前任务；
  - 识别到其它指令 → 打断后立即执行新指令。
- 派发完任务：主 Agent 立即语音汇报"任务已派发"，随后回到待命监听状态。
- 子 Agent 完成：自动调主 Agent 用语音向用户汇报结果。
- 主 Agent 每次回复至少调用一次 `SpeakToUser` 工具（TTS 后端可在设置中切换 internal / goose-tts）。

## Skills 系统

- 每个 skill 是一个文件夹，内含运行文件 + 说明文档（README.md / SKILL.md）。
- 可在 UI「Skills」页手动导入路径，也可让主 Agent 通过 `ImportSkill` 工具自动导入。
- skill 说明会拼入双 Agent 的 system prompt，供模型按需调用。

## API 一览

| 方法 | 路径 | 说明 |
|------|------|------|
| GET  | /health | 健康检查 |
| GET  | /api/status | 主/子 Agent 状态、语音连接、唤醒词 |
| GET  | /api/events | SSE 事件流（对话/工具/语音/子任务/记忆/Skills/定时） |
| POST | /api/chat | 发送文本给主 Agent |
| POST | /api/chat/interrupt | 打断主 Agent |
| POST | /api/voice/wakeword | voice-serve 唤醒词回调 |
| POST | /api/voice/beep | 播放提示音 |
| POST | /api/voice/listening | 开关监听 |
| POST | /api/voice/interrupt | 打断当前 TTS |
| GET/POST | /api/config | 读写配置（保存后重建 Agent） |
| GET/POST | /api/skills · /api/skills/import · /api/skills/:name · /api/skills/:name/toggle | Skills 管理 |
| GET/POST | /api/memory · /api/memory/:id · /api/memory/clear | 记忆管理 |
| GET/POST | /api/schedules · /api/schedules/:id | 定时任务（cron 5 字段「分 时 日 月 周」） |
| GET      | /api/tasks | 子 Agent 任务登记表 |
| POST     | /api/admin/rebuild | 重建双 Agent |

## 版本

- **1.0.0**：首个公开发布版本。完整 Rust 语音闭环（KWS + Paraformer STT + EdgeTTS / goose-tts）、
  双 Agent 编排、Skills / 记忆 / 定时任务 / 会话管理、内置 searchpin 搜索，Electron 纯白 UI。

## 许可证

本项目基于 [Apache-2.0](./LICENSE) 协议开源。vendored 的 open-agent-sdk-rust
（crates/agents）为 [codeany-ai/open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust)
的独立开源框架，遵循其自身 MIT 协议。
