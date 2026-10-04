---
AIGC:
    Label: "1"
    ContentProducer: 001191440300708461136T1XGW3
    ProduceID: abc280594deb8e830e487cdb027f0632_f49920bab5bf11f1a816525400cd780f
    ReservedCode1: Rxp3KJKE0L3kY2tVvIPGRoKc2RItw11TMvF3IbcHwlBcJfknvuLV6Y3y4nTXn0PK3rx0J0eWR4UTveV1FhCZJ8wcxMcd4NC2pSne/nAILYitrgKJFqAmfHjypU3Ozwm/WoPevid6PDmKcylkzLA+5QyaaRQF6r7b/zvdBb6PR5cchhO4eVLjshC6DwI=
    ContentPropagator: 001191440300708461136T1XGW3
    PropagateID: abc280594deb8e830e487cdb027f0632_f49920bab5bf11f1a816525400cd780f
    ReservedCode2: Rxp3KJKE0L3kY2tVvIPGRoKc2RItw11TMvF3IbcHwlBcJfknvuLV6Y3y4nTXn0PK3rx0J0eWR4UTveV1FhCZJ8wcxMcd4NC2pSne/nAILYitrgKJFqAmfHjypU3Ozwm/WoPevid6PDmKcylkzLA+5QyaaRQF6r7b/zvdBb6PR5cchhO4eVLjshC6DwI=
---

# Star Trek Computer · 星际迷航语音助手（Rust 版）

**版本 1.0.0** · Apache-2.0

一个完整的星际迷航风格本地语音助手。Rust 后端 + Electron 纯白极简 UI，
纯 Rust 语音闭环（KWS 唤醒 / STT / TTS），支持双 Agent 编排、Skills、记忆、定时任务
与内置多引擎联网搜索（四引擎零 Key，无需外部二进制）。

项目自包含：vendored open-agent-sdk-rust 已并入 `crates/agents/`，克隆后即可编译运行，
无需额外拉取外部 crate 或配置本地 path。

---

## 功能特性

- **语音交互**：喊 `computer` 唤醒，支持打断 / 停止关键词；TTS 播报（EdgeTTS internal / goose-tts 外部二进制）
- **双 Agent 架构**：主 Agent（精简工具：联网搜索 + 派发 / 监控 / 打断子 Agent + 语音播报 + 导入 Skill）
  + 子 Agent（完整工具能力，后台执行，完成后自动语音汇报）
- **内置搜索**：`WebSearchTool` 内嵌多引擎联网搜索（Bing 国际/国内、百度、搜狗，四引擎并行 + 词法重排，零 API Key），
  无需任何外部二进制，各引擎独立退避限流
- **Skills 系统**：文件夹即 skill，可 UI 导入或主 Agent 自动导入
- **记忆 / 定时任务 / 会话管理**：HTTP API + SSE 事件流全量支持
- **Electron UI**：纯白极简界面（对话 / 记忆 / Skills / 定时 / 子任务 / 设置）

---

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

---

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
│  │     ├─ search.rs               内嵌多引擎联网搜索（WebSearch 工具）
│  │     ├─ sessions.rs             会话管理
│  │     ├─ skills.rs / memory.rs / scheduler.rs / events.rs / state.rs / http.rs / voice.rs
│  └─ voice/                        voice-serve 语音进程（KWS/STT/TTS/提示音）
├─ ui/                              Electron 界面（package.json + electron/ + src/）
├─ resources/                       运行资源：音效、goose-tts（模型权重不入库，见「模型与依赖放置说明」）
├─ data/                            运行时数据（不入库，需按 README 配置；打包版落到用户可写目录）
├─ run.sh                           一键启动脚本
└─ LICENSE                          Apache-2.0
```

---

## 可移植性（相对路径解析）

项目**不硬编码任何用户绝对路径**，克隆到任意电脑 / 任意位置均可编译运行：

- **运行时资源**统一基于「项目根 `/resources`」相对解析（`crates/core/src/paths.rs`），
  包括 `goose-tts`、`wake_sound.wav` / `complete.mp3` 等音效。
- **项目根定位顺序**：环境变量 `STAR_TREK_ROOT` → 当前可执行文件向上定位（`<root>/target/<profile>/<bin>`）→ 当前工作目录。
  打包后的 `.app` 由 Electron 自动注入 `STAR_TREK_ROOT=process.resourcesPath`。
- **历史配置自动迁移**：首次启动若 `data/config.json` 中仍残留旧绝对路径（如 `/Users/xxx/Projects/star-trek-assistant/...`），
  core 会自动改写为相对形式（`resources/...`、`data`）并回写，随后基于当前项目根解析使用，无需手工改配置。
- **HOME 兜底**统一为通用行为（取 `$HOME`，取不到时回退项目根），不再假设用户名。
- **环境变量覆盖**（均可选）：`STAR_TREK_ROOT`（项目根）、
  `KWS_DIR` / `MODEL_DIR`（语音模型目录）、`BEEP_FILE`（提示音）、`SHERPA_ONNX_LIB_DIR`（编译期 sherpa-onnx 库）。

---

## 模型与依赖放置说明

语音链路（唤醒 + STT）需要 sherpa-onnx 模型，项目本身**不内置模型文件**，按以下方式之一放置：

### 方式 A：放进项目内（推荐，随项目相对解析）

```
resources/models/
├─ kws/                             # KWS 唤醒词模型（如 sherpa-onnx-kws-zipformer 系列）
│  └─ ...（含 encoder/decoder/joiner 等 .onnx）
└─ paraformer-zh/                   # STT 模型（sherpa-onnx-paraformer-zh-2023-09-14）
   └─ ...（含 model.onnx、tokens.txt 等）
```

voice-serve 启动时会**优先检查** `resources/models/kws` 与 `resources/models/paraformer-zh`；
存在则直接使用，无需任何配置。模型放置好后 `cargo build --release` 并重启即可。

> **模型权重（`*.onnx`）不随仓库分发**（体积达数百 MB）。仓库内仅保留下载脚本与配置：
> - STT（Paraformer-zh）：`python3 resources/models/paraformer-zh/download-model.py`
>   （需先 `pip install modelscope`；下载后将 `model_quant.onnx` 重命名为 `model.int8.onnx` 放入该目录）。
> - KWS：从 sherpa-onnx 官方发布获取对应 zipformer KWS 模型，放入 `resources/models/kws/`。
> 也可直接使用打包版 macOS App（已内置模型与二进制，无需自行下载）。

### 方式 B：环境变量指向外部目录

```bash
export KWS_DIR=/path/to/kws-model-dir
export MODEL_DIR=/path/to/sherpa-onnx-paraformer-zh-2023-09-14
export BEEP_FILE=/path/to/wake_sound.wav   # 可选，默认 resources/wake_sound.wav
./run.sh
```

> 历史版本默认指向 `~/Projects/goose` 与 `~/Projects/funasr-cli` 下的模型目录；
> 换机后若目录不存在，请按方式 A / B 重新放置或设置，voice-serve 会在缺失时给出明确日志。

### sherpa-onnx 静态库（编译依赖）

- voice-serve 依赖 `sherpa-onnx` prebuilt 静态库。`SHERPA_ONNX_LIB_DIR` **环境变量优先**；
  未设置时回退到本机默认路径（`.cargo/config.toml`，`force = false` 不覆盖已有环境变量）。
- 换机 / 换路径：`export SHERPA_ONNX_LIB_DIR=<sherpa-onnx lib 目录>` 后重新 `cargo build --release`；
  也可不设置，让 sherpa-onnx 构建脚本自动下载对应平台 prebuilt。

---

## 打包 macOS App（electron-builder）

`ui/`（Electron 31）可打包为可分发的 macOS arm64 `.app`：

```bash
cd ui && npm install && npx electron-builder --mac --arm64 --publish never
```

- **产物**：`ui/dist/Star Trek Computer-darwin-arm64/Star Trek Computer.app`（或 `ui/dist/*.dmg`/`*.zip`，按 `package.json` 配置）。
- **打包内容（extraResources）**：
  - `resources/` → `Star Trek Computer.app/Contents/Resources/resources/`（goose-tts、音效、模型）；
  - `target/release/star-trek-core`、`target/release/voice-serve` → `.../Contents/Resources/bin/`。
- **打包后路径适配**（`ui/electron/main.js`）：
  - `PROJECT_ROOT = process.resourcesPath`（asar 内不可写，资源全部放 resourcesPath）；
  - `data/` 与日志落到**用户可写目录** `~/Library/Application Support/Star Trek Computer/data`（`app.getPath('userData')`）；
  - core 启动时注入 `STAR_TREK_ROOT=process.resourcesPath`，后端自动拉起 voice-serve（同目录 `bin/`）。
- **macOS 麦克风 TCC**：adhoc 签名可能拿不到麦克风权限；如遇此问题，用自签证书对 `.app` 深度重签：
  ```bash
  codesign --force --deep --sign "<你的自签证书名>" "Star Trek Computer.app"
  ```
  首次启动时在「系统设置 → 隐私与安全性 → 麦克风」中允许该 App 访问麦克风。

---

## 快速开始

```bash
# 1. 克隆仓库（已包含 vendored open-agent-sdk-rust，clone 后即可编译）
git clone https://github.com/zhj-ldm/Star-Trek-voice-computer-rust.git
cd Star-Trek-voice-computer-rust

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

---

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

---

## 语音交互（双唤醒场景）

- 主 Agent 空闲：喊 `computer` → 提示音 → 录音转文字 → 作为指令发送给主 Agent。
- 主 Agent 干活中：喊 `computer` → 打断当前 TTS → 录音转文字；
  - 识别到 `stop / 停止 / 停 / 够了 / 取消`（可配置）→ 仅打断当前任务；
  - 识别到其它指令 → 打断后立即执行新指令。
- 派发完任务：主 Agent 立即语音汇报"任务已派发"，随后回到待命监听状态。
- 子 Agent 完成：自动调主 Agent 用语音向用户汇报结果。
- 主 Agent 每次回复至少调用一次 `SpeakToUser` 工具（TTS 后端可在设置中切换 internal / goose-tts）。
- 语音播报硬约束：未播报不许结束，`speak_retry < 3` 时强制重播，确保每次回复都能听到语音。

---

## Skills 系统

- 每个 skill 是一个文件夹，内含运行文件 + 说明文档（README.md / SKILL.md）。
- 可在 UI「Skills」页手动导入路径，也可让主 Agent 通过 `ImportSkill` 工具自动导入。
- skill 说明会拼入双 Agent 的 system prompt，供模型按需调用。

---

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

---

## 常见问题

**Q: 唤醒后没有声音 / 听不到播报？**
检查 `data/config.json` 中 `voice_enabled` 是否为 `true`，以及 `tts_backend` 是否配置正确；
internal 后端需联网（edge-tts），goose-tts 后端需 `resources/goose-tts` 文件存在且有执行权限。

**Q: 麦克风没有声音？**
首次使用需在 macOS「系统设置 → 隐私与安全性 → 麦克风」中允许终端（或承载进程）访问麦克风；
建议通过终端运行 `./run.sh` 启动。

**Q: 内置搜索不可用 / 无结果？**
内置搜索不依赖任何 API Key 与外部二进制，四引擎并行抓取；若某个引擎触发反爬限流会自动退避重试，
稍后再试或换个关键词即可。

**Q: 编译时 sherpa-onnx 报错？**
参见上方「编译前置说明」，设置 `SHERPA_ONNX_LIB_DIR` 指向本地 prebuilt 的 lib 目录。

---

## 版本

- **1.0.0**：首个公开发布版本。完整 Rust 语音闭环（KWS + Paraformer STT + EdgeTTS / goose-tts）、
  双 Agent 编排、Skills / 记忆 / 定时任务 / 会话管理、内置多引擎联网搜索，Electron 纯白 UI。

---

## 许可证

本项目基于 [Apache-2.0](./LICENSE) 协议开源。vendored 的 open-agent-sdk-rust
（crates/agents）为 [codeany-ai/open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust)
的独立开源框架，遵循其自身 MIT 协议。
*（内容由AI生成，仅供参考）*
