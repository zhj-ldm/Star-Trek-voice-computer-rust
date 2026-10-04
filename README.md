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

# Star Trek Computer · 星际迷航语音电脑（Rust 版）

Apache-2.0 · 本地语音助手：**纯 Rust 语音闭环**（KWS 唤醒 / 打断 / ASR / TTS）+ 单 Agent 编排
+ Skills + 记忆 + 定时任务 + 内置多引擎联网搜索 + Electron 极简 UI。

唤醒后直接说话即可；语音采集完全在 Rust 后端（cpal）完成，渲染进程只做 UI。

---

## 组成（workspace）

| crate | 产物 | 说明 |
|-------|------|------|
| `crates/core` | `star-trek-core` | 主后端：Agent 编排 + HTTP API + SSE + 语音进程拉起 |
| `crates/voice` | `voice-serve` | 独立语音进程：常驻麦克风采集 + KWS + ASR + TTS + 个人日志 |
| `crates/journal` | `journal` | 个人日志系统 skill 的 CLI（单二进制，走 voice-serve） |
| `crates/agents` | 库 `open-agent-sdk` | vendored 的 [open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust)（MIT），core 的 Agent 框架 |
| `crates/aggrsearch` | — | 独立实验 crate（聚合搜索服务），**不在 workspace 内**，不影响主程序编译 |

---

## 功能特性

- **语音闭环**（`voice-serve`）
  - 唤醒词 **KWS**（sherpa-onnx，默认 `computer`）；整句打断词（默认 `computer stop`）
  - **ASR**：sherpa-onnx **Paraformer-zh**（FunASR 同源），替代 whisper
  - **TTS**：`edge-tts-rust`（内置）或外部二进制 `goose-tts`；播报音色 / 语速可配
  - 提示音；播报期间自保护（不自唤醒）、播报结束清缓冲
  - **常驻采集 + fan-out 广播**：唤醒、日志、一次性识别共用同一条 cpal 采集流
- **Agent**：主 Agent（工具：`ImportSkill`、内置 `WebSearch`、可选 `AskUserQuestion`，其余 bash/文件/WebFetch/Tasks 由 SDK 默认注册）；子 Agent 用于后台任务
- **语音播报**：每轮结束由 core 用最终回复自动播报（取 `【Voice】` 段落，自动清洗 Markdown），**不再作为 AI 工具**
- **多上游分摊**：`providers` 支持多 Base URL / 多模型 / 多 API Key，round-robin 轮询并按上游独立 RPM 限流；`active_model` 切换当前模型
- **Skills / 记忆 / 定时任务 / 多会话**：HTTP API + SSE 事件流全量支持
- **Electron UI**：对话 / 记忆 / Skills / 定时 / 子任务 / 设置（含唤醒日志面板）

---

## 模型接入协议（重要）

程序通过 `open-agent-sdk` 调用大模型，**支持两种接入协议，二选一自动判定**：

| 协议 | 端点（由 Base URL 拼出） | 鉴权头 | 判定条件 |
|------|--------------------------|--------|----------|
| **OpenAI Chat Completions** | `POST {Base URL}/v1/chat/completions` | `Authorization: Bearer <key>` | 模型名命中：`gpt-` / `o1` / `o3` / `o4` / `deepseek` / `qwen` / `yi-` / `glm` / `mistral` / `gemma` / `mimo` / `llama` / `gemini` |
| **Anthropic Messages** | `POST {Base URL}/v1/messages` | `x-api-key` + `anthropic-version` | 以上都不命中时的**默认值** |

要点：

- **不是 OpenAI Responses API**（不使用 `/v1/responses`）。OpenAI 侧走的是标准的 **Chat Completions**。
- **斜杠与 `/v1` 写法**：Base URL 会被归一化——先去掉**末尾斜杠**，再去掉**末尾 `/v1`**，然后统一拼上 `/v1/...`。
  因此下面这些写法**等价、都能用**：
  - `https://api.openai.com` ✅
  - `https://api.openai.com/v1` ✅（推荐，最直观）
  - `https://api.openai.com/v1/` ✅
- **本地模型**（如 Ollama）填 `http://127.0.0.1:11434`（可省 `/v1`），本地地址会自动走「关闭思考」等适配逻辑。
- **强制协议**：若你的模型名不在上面的启发式列表里（例如自建网关的自定义模型名），别被默认的 Anthropic 判定带偏——
  设环境变量 `CODEANY_API_TYPE=openai-completions`（或 `anthropic-messages`）即可强制。
- 环境变量兜底：`CODEANY_API_KEY` / `CODEANY_BASE_URL` / `CODEANY_MODEL`。

> UI 设置页「Base URL」输入框的占位提示即按上面规则给出标准示例 `https://api.openai.com/v1`。

---

## 目录结构

```
star-trek-assistant/
├─ Cargo.toml / Cargo.lock          workspace（members: core / voice / journal）
├─ crates/
│  ├─ core/                         star-trek-core：Agent 编排 + HTTP/SSE + 拉起 voice-serve
│  │  └─ src/{main,config,agents,tools,search,sessions,skills,memory,scheduler,events,state,http,voice,paths}.rs
│  ├─ voice/                        voice-serve：KWS / ASR / TTS / 采集 / 个人日志（journal.rs）
│  ├─ journal/                      个人日志 skill 的 CLI（单二进制）
│  ├─ agents/                       vendored open-agent-sdk（MIT）
│  └─ aggrsearch/                   独立实验 crate（不在 workspace）
├─ Skills/                          运行在本程序之上的技能（见 Skills/README.md）
│  └─ personal-journal/             个人日志系统技能（SKILL.md + config.json + journal 二进制）
├─ ui/                              Electron 界面（package.json + electron/ + src/）
├─ resources/                       运行资源：音效 / goose-tts / prompts / models（模型权重不入库）
├─ data/                            运行时数据（不入库，含 API Key）
├─ run.sh                           一键启动
└─ update-app.sh                    编译并覆盖桌面 .app（含重签）
```

---

## 模型与依赖放置说明

语音链路（KWS + ASR）需要 sherpa-onnx 模型，**仓库不内置模型权重**（`resources/models/**/*.onnx` 已被
`.gitignore` 排除，体积数百 MB），仅保留词表 / 配置 / 下载脚本 / 测试音频。放置方式二选一：

**方式 A：放进项目内（推荐）**

```
resources/models/
├─ kws/                 # 唤醒词模型（sherpa-onnx-kws-zipformer 系列；含 encoder/decoder/joiner .onnx）
│  ├─ tokens.txt
│  └─ keywords_computer.txt / keywords_break.txt
└─ paraformer-zh/       # ASR 模型（sherpa-onnx-paraformer-zh）
   ├─ model.int8.onnx   # 权重（不入库）
   └─ tokens.txt 等
```

voice-serve 启动时优先检查 `resources/models/kws` 与 `resources/models/paraformer-zh`，存在即用。
STT 权重可用 `python3 resources/models/paraformer-zh/download-model.py` 下载后再放到该目录。

**方式 B：环境变量指向外部目录**

```bash
export KWS_DIR=/path/to/kws-model-dir
export MODEL_DIR=/path/to/sherpa-onnx-paraformer-zh
export BEEP_FILE=/path/to/wake_sound.wav     # 可选
./run.sh
```

**编译依赖**：`voice-serve` 依赖 `sherpa-onnx` prebuilt 静态库。`SHERPA_ONNX_LIB_DIR` 环境变量优先；
未设置时构建脚本会自动下载对应平台 prebuilt。

---

## 快速开始

```bash
# 1. 克隆（已含 vendored open-agent-sdk，clone 后即可编译）
git clone https://github.com/zhj-ldm/Star-Trek-voice-computer-rust.git
cd Star-Trek-voice-computer-rust

# 2. 编译后端（core + voice-serve + journal）
cargo build --release

# 3. 放置语音模型（见上「模型与依赖放置说明」）

# 4. 安装 UI 依赖并启动（自动拉起后端 + 语音进程）
cd ui && npm install && cd ..
./run.sh
```

> 建议在**终端**运行 `./run.sh` 以继承 macOS 麦克风（TCC）授权；首次需在
> 「系统设置 → 隐私与安全性 → 麦克风」中允许终端访问麦克风。

---

## 配置

运行数据落在 `data/`（不入库，含 API Key）。主配置 `data/config.json` 主要字段：

```json
{
  "data_dir": "data",
  "providers": [
    { "name": "主 API", "base_url": "https://api.openai.com/v1", "model": "gpt-4o",
      "api_key": "YOUR_KEY", "api_keys": [], "rpm_limit": 20 }
  ],
  "active_model": "主 API",
  "main_base_url": "https://api.openai.com/v1",
  "main_api_key": "YOUR_KEY",
  "main_model": "gpt-4o",
  "main_system_prompt": "（人设，追加在系统规则之后）",
  "voice_port": 8420,
  "wakeword": "computer",
  "beep_file": "resources/wake_sound.wav",
  "voice": "zh-CN-XiaoxiaoNeural",
  "rate": 1.05,
  "tts_backend": "goose-tts",
  "goose_tts_path": "resources/goose-tts",
  "interrupt_keywords": ["stop", "停止", "停", "够了", "取消"],
  "max_record_secs": 120.0,
  "kws_threshold": 0.25,
  "voice_enabled": true,
  "skill_dirs": [],
  "max_turns": 1000,
  "rpm_limit": 20,
  "enable_thinking": false,
  "ask_user_enabled": true
}
```

- `providers` 非空时优先：多通道轮询分摊 RPM；每套配置 = 一个「URL + 模型名 + 多 Key」。
  `active_model` 匹配 `name` 选定当前模型。
- 以上均可在 UI「设置」页可视化编辑（Base URL / API Key / 模型 / 系统提示词 / 语音阈值 / TTS 等）。

---

## 语音交互

- 主 Agent 空闲：喊 `computer` → 提示音 → 录音转文字 → 作为指令发给主 Agent。
- 主 Agent 干活 / 播报中：喊 `computer` 后再补 `stop`（整句 `computer stop`）→ 打断当前任务。
- 播报期间不自唤醒（判定为自身声音回授）；播报结束清采集缓冲。
- 每轮结束 core 自动把最终回复（`【Voice】` 段）用 TTS 播报。

---

## 个人日志系统（Skills）

见 [`Skills/README.md`](Skills/README.md)。开启后持续录音（复用同一路采集），整场一个音频文件
（静音不落盘），后台自动转写为同名 Markdown；内置 ASR / KWS 以 CLI 开放给技能。

---

## API 一览

**core（默认 `127.0.0.1:8410`）**

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` · `/api/status` | 健康检查 / 状态 |
| GET | `/api/events` | SSE 事件流（对话 / 工具 / 语音 / 子任务 / 记忆 / Skills / 定时） |
| POST | `/api/chat` · `/api/chat/interrupt` | 发送文本 / 打断 |
| POST | `/api/voice/wakeword` · `/api/voice/beep` · `/api/voice/listening` · `/api/voice/interrupt` | 语音回调 |
| GET/POST | `/api/config` | 读写配置 |
| GET/POST | `/api/skills` · `/api/skills/import` · `/api/skills/:name` · `/api/skills/:name/toggle` | Skills 管理 |
| GET/POST | `/api/memory` · `/api/memory/:id` · `/api/memory/clear` | 记忆 |
| GET/POST | `/api/schedules` · `/api/schedules/:id` | 定时任务（cron 5 字段） |
| GET | `/api/tasks` | 子任务登记表 |
| GET/POST | `/api/sessions` · `/api/sessions/:id` · `.../rename` · `.../switch` · `.../messages` · `.../clear` | 会话 |
| POST | `/api/admin/rebuild` · `/api/system/open-mic-settings` | 运维 |

**voice-serve（默认 `127.0.0.1:8420`）**

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` · `/status` · `/is_speaking` · `/kws_diag` | 状态 / 唤醒日志 |
| POST | `/speak` · `/interrupt` · `/beep` | TTS / 打断 / 提示音 |
| POST | `/listen_once` · `/transcribe` · `/wakeword_once` · `/wakeword_detect` | 识别 / 检测 |
| POST | `/kws_reset` · `/kws_config` · `/listening` · `/reinit_capture` | 唤醒配置 / 监听 / 重建采集 |
| POST/GET | `/journal/start` · `/journal/stop` · `/journal/status` · `/journal/dir` · `/journal/write` · `/journal/stt_file` · `/journal/kws_file` | 个人日志 |

---

## 可移植性

项目不硬编码用户绝对路径：运行时资源基于「项目根 / `resources`」相对解析（`crates/core/src/paths.rs`）；
项目根定位顺序为 `STAR_TREK_ROOT` → 可执行文件向上定位 → 当前工作目录；历史绝对路径配置会自动迁移为相对形式。

---

## 打包 macOS App

`ui/`（Electron）可打包为 macOS `.app`；本机亦可用 `./update-app.sh` 编译 release 并覆盖桌面
`Star Trek Computer.app`（含自签重签）。macOS 麦克风 TCC：如遇权限问题，用自签证书对 `.app` 深度重签。

---

## 许可证

本项目基于 [Apache-2.0](./LICENSE) 开源。vendored 的 `crates/agents`（open-agent-sdk-rust）为
[codeany-ai/open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust) 的独立开源框架，遵循其自身 MIT 协议。
