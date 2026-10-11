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

# Star Trek Computer · 星际迷航语音电脑

> 喊一声 `computer`，它醒了。说完直接给指令，全程不用碰键盘。

**唤醒和语音识别 100% 在你本机跑，不装 Python、不装 conda、不配虚拟环境。**
后端全部是 Rust 二进制：`cargo build --release` 出来的就是可执行文件。

单 Agent 编排 + Skills + 长期记忆 + 定时任务 + 内置多引擎联网搜索 + Electron 极简界面，
macOS 可打包成菜单栏常驻的 `.app`。

---

## 这是个什么项目

一个**桌面语音助手**，工作方式就三步：

```text
你说 "computer"
      ↓
本机 KWS 模型确认唤醒（不联网）
      ↓
你继续说指令，本机 ASR 转成文字（不联网）
      ↓
文字发给大模型 → 回复用 TTS 念给你听
```

关键点：**唤醒和识别这两步完全本地**。不上传音频，不依赖云端 ASR，也不需要联网才能唤醒。
只有"理解指令"和"念回复"两步需要网络。

所以它不是一个"套壳聊天客户端"，语音入口是一套自己实现的 Rust 语音前端。

---

## 依赖边界（先说清楚，避免误解）

| 环节 | 跑在哪 | 需要网络 | 说明 |
|------|--------|---------|------|
| 麦克风采集 | 本机 | ❌ | cpal / CoreAudio |
| 唤醒 KWS | **本机** | ❌ | sherpa-onnx Zipformer，约 5 MB |
| 语音识别 ASR | **本机** | ❌ | sherpa-onnx Paraformer-zh，约 234 MB |
| 大模型推理 | 云端 / 你自己的端点 | ✅ | OpenAI 或 Anthropic 协议均可 |
| 语音合成 TTS | 云端 | ✅ | Edge TTS |
| 联网搜索 | 云端 | ✅ | Bing / 百度 / 搜狗多引擎聚合 |

**准确说法**：这不是"全离线"语音助手，LLM 和 TTS 仍在云端。
但它做到了**语音入口零 Python 运行时依赖**——不需要 Python、pip、conda、venv，
不需要 `pip install funasr`，不需要编译 ffmpeg 依赖树。

> 关于"全 Rust"的准确说明：本项目的**业务与胶水代码全部是 Rust**，
> 语音模型通过 `sherpa-onnx` 这一 Rust crate 调用，底层是 ONNX Runtime（同样是原生库，不是 Python）。
> 严格说没有一个"纯 Rust 实现"，但**运行时确实一个 Python 解释器都不需要**——
> 这正是它和绝大多数 Python 语音助手方案最实际的差别。

---

## 和常见 Python 语音方案的实际差别

### 1. 装完就跑，不用先修环境

Python 方案常见的坑：`conda` 版本对不上、`funasr` 依赖 `torch` 装了几个 GB、
`pip install` 编译 `soundfile`/`webrtcvad` 失败、换台机器全部重来。
本项目 `cargo build --release` 一条命令，编出来就是二进制。

### 2. 音频不上传

唤醒和转写都在本机完成。麦克风数据不出机器，只有文字发给模型。
对隐私敏感的场景（会议记录、个人日志）这一点是硬需求，不是加分项。

### 3. 唤醒模型很小，CPU 就够

KWS 模型约 5 MB，检测窗口 2.4 s @16 kHz，Zipformer Transducer 流式解码，
笔记本 CPU 上单次推理在毫秒级。不需要 GPU，也不需要把整段音频送出去。

### 4. 环境噪声下的实际处理

代码里不是"直接把麦克风数据丢给模型"，而是走了一条完整的前端链路（下面有专门一节说明）。
结果是小声说话也能唤醒，稳态底噪不会反复误触。

### 5. 播报不会把自己唤醒

TTS 播放的声音会被麦克风收回来。本项目做了三重处理：播报期间不判唤醒、
播报结束瞬间清空采集缓冲、命中后 2.6 s 抑制窗口。这是个看起来小但实际天天遇到的问题。

### 6. 唤醒不响应时，能查出原因

`GET /kws_diag` 暴露最近 300 条检测记录：窗口样本数、RMS、噪声底、门限、增益、推理耗时、是否命中。
唤醒失灵不用瞎猜，对着这几个数就能定位是采集、门控还是模型的问题。

---

## 语音链路是怎么处理的

这一节是本项目在语音前端上花心思最多的地方，也是它和"能跑就行"的 demo 的区别。

```text
cpal 麦克风采集
    ↓
多声道 → 单声道
    ↓
Butterworth 二阶低通 @ 7 kHz          抗混叠
    ↓
相位连续重采样 → 16 kHz                消除时间轴抖动
    ↓
自适应噪声底 + RMS 静音门控            跳过纯静音
    ↓
电平归一化 + 峰值约束                  稳定的模型输入电平
    ↓
sherpa-onnx KWS（100 ms 分块流式）     真正的"有人叫我吗"
    ↓
sherpa-onnx Paraformer-zh ASR          文字
```

### 抗混叠：为什么先滤波再降采样

麦克风常见输入是 44.1 kHz，KWS/ASR 要的是 16 kHz。
如果直接每 2.756 个点抽一个，8 kHz~22 kHz 的频率会**折返回 0~8 kHz 语音带**，
直接污染 KWS 置信度和能量估计。所以在抽取之前先用二阶 Butterworth 低通压到 7 kHz：

```rust
// crates/voice/src/audio.rs
struct Biquad { b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, x1: f32, x2: f32, y1: f32, y2: f32 }

fn lowpass(fs: f32, fc: f32) -> Self { /* 标准 RBJ biquad，Q = 1/√2 */ }
```

### 相位连续：为什么不能每个回调从 0 重采样

音频回调是一帧一帧来的。如果每帧都从索引 0 开始插值，采样相位每秒跳变几十次，
相当于给音频人为叠了一层时间轴抖动，唤醒和识别都会受影响。
所以重采样器把 `pos` 跨回调保留：

```rust
struct Resampler { step: f64, pos: f64, last: f32, has_last: bool }
```

### 自适应噪声底 + RMS 门控：它到底在做什么

这是本项目里最容易被误解的一段，所以这里说准确：

```rust
// crates/voice/src/main.rs —— 每 300 ms 一轮（AI 忙碌时 200 ms）

// 1) 整窗 RMS
let rms_sqrt = (win.iter().map(|s| s * s).sum::<f32>() / win.len().max(1) as f32).sqrt();

// 2) 自适应噪声底：比它低就即时下探，否则每 tick 上抬 1%（约 35 s 跟随环境）
if !hit_prev && !is_busy_now {
    if rms_sqrt < noise_floor { noise_floor = rms_sqrt; }
    else { noise_floor *= 1.01; }
}

// 3) 静音门控：低于门限直接跳过这一轮
let gate = (noise_floor * 3.0).max(0.0018);
if rms_sqrt < gate { continue; }

// 4) 电平归一化 + 峰值约束（三者取最小，绝不削顶）
let peak = win.iter().fold(0.0f32, |m, s| m.max(s.abs()));
let gain = (0.08f32 / rms_sqrt).min(40.0).min(0.95 / peak.max(1e-6));
```

这里有四个值得注意的设计点：

- **噪声底跟随速度刻意不对称**：向下瞬时（换环境立刻跟上），向上每 tick 只抬 1%
  （约 35 s 才收敛）。早期版本上抬权重过小，环境变了门限要几十秒才跟上，
  表现就是"怎么调参数都一会儿好用一会儿不好用"。门控和增益互相耦合、跟随速度不匹配，
  是这类系统不稳定的常见根因。
- **增益不是估计出来的，是直接算出来的**：目标 RMS 0.08，上限 40×，
  再用 `0.95 / peak` 保证峰值不超过 0.95。早期版本只 clamp 到 ±1，
  遇到爆破音/瞬态会削顶失真，导致偶发漏检。
- **静音门控只做一件事**：跳过纯静音，**不参与增益估计**。两件事耦合在一起就很难调。

### 关于 RMS 和 VAD 的关系（常见误解澄清）

网上不少方案会把 RMS 换成 Silero VAD 或 WebRTC VAD，理由是"更准"。
这里要说清楚差别：

| | RMS 门控（本项目） | Silero / WebRTC VAD |
|---|---|---|
| 输出 | 标量能量值 | 人声概率 0~1 |
| 能改音频电平 | ✅ 直接参与增益计算 | ❌ 只输出概率 |
| 解决"这段有没有声音" | 粗（能量） | 细（区分人声/噪声） |
| 额外依赖 | 无 | Python / ONNX 模型 / GMM 权重 |

**它们解决的是不同问题，不是替代关系。**
VAD 能更准地分辨"是人声还是空调噪声"，但它**不改变音频本身的电平**——
而 Zipformer KWS 对输入电平是有期望的。所以本项目的做法是：

- 用 RMS 负责**电平归一化 + 静音门控**，保证模型每次拿到的输入电平稳定；
- 真正的"这句话是不是 computer"，交给 KWS 模型自己判断——它本来就是专门做这件事的。

换句话说，**这里没有"自研 RMS 替代了 Silero VAD"这回事**，
能量前端和识别模型各司其职。如果后续要加 VAD，它是作为**额外的一层前置过滤**
（跳过非人声段、省算力、降低噪声段误触），而不是替换 RMS 的增益作用。

### 唤醒窗口为什么是 2.4 s

```rust
const WIN_SAMPLES: usize = 16000 * 24 / 10;   // 2.4 s @16k
const BREAK_WIN_SAMPLES: usize = 16000 * 3;   // 3.0 s @16k（整句 "computer stop"）
const BREAK_ARM: Duration = Duration::from_millis(2500);
const SUPPRESS: Duration = Duration::from_millis(2600);
```

这些数字不是拍脑袋定的，是离线实测出来的：

- 单字 `computer`：1.2 s / 1.8 s / 2.4 s / 3.0 s 窗口全部稳定命中，音量降到 0.1× 仍命中。
  取 2.4 s 是为了给流式 Zipformer（chunk-16-left-64）更多左上下文，降低跨窗口边界的丢词概率。
- 整句 `computer stop`（0.95 s）：1.2 s 窗口 **100% 漏检**，1.8 s 只有一半命中，
  2.4 s / 3.0 s 才 100% 命中。**整句关键词必须配长窗口，这是硬约束**。

所以打断检测器是**独立的第二个检测器 + 独立关键词表**，只在听到 `computer` 后的 2.5 s 内才运行。
原因很实际：sherpa KWS 单检测器命中第一个词就返回，`computer stop` 不可能靠"先武装再等第二个词"实现。
平时不跑打断检测，省一半算力。

`SUPPRESS = 2.6 s`（略大于 2.4 s 窗口）也很关键：
如果唤醒后只冷却 1 s，同一句 `computer stop` 还在窗口内，
AI 已被打断变成空闲，残留的 `computer` 就会被当成新唤醒——实测表现为"每次打断后约 1.26 s 必有一次空闲态唤醒命中"。

### KWS 触发式检测的一个坑

```rust
// crates/voice/src/wakeword.rs
let chunk_len = (self.sample_rate as usize / 10).max(1);  // 100 ms 分块
for chunk in samples.chunks(chunk_len) {
    stream.accept_waveform(chunk);
    // 关键：每块之后立刻取结果，不能只在 input_finished 后取一次
}
```

KWS 是触发式检测，命中状态必须及时取走，否则会被后续输入覆盖掉、返回 `None`。
100 ms 分块也对齐真实麦克风节奏，而不是一次性灌一整段。

### 防自唤醒（三重）

1. **播报期间不判唤醒**，只保留整句 `computer stop` 打断能力；
2. **播报结束那一瞬间**捕捉到 `speaking: true → false` 的跳变，立刻 `capture.discard()` 清空缓冲，
   让残留的"自己的声音"彻底滑出，而不是等它自己漂出窗口；
3. **命中后 2.6 s 抑制**，让刚说出口的那句话完全滑出检测窗口。

第 2 点尤其重要——只在播报结束后"开始判"，但缓冲里还留着 2.4 s 的旧音频，
照样会把自己唤醒。

### 打断为什么能"即时"

```rust
// crates/voice/src/tts.rs
interrupted: Arc<AtomicBool>,

pub fn interrupt(&self) {
    self.speaking.store(false, Ordering::SeqCst);
    self.interrupted.store(true, Ordering::SeqCst);
}
```

播放跑在独立 OS 线程上，阻塞在解码/输出时 mpsc 队列里的命令送不进去，
所以打断**绕过队列直接置共享标志**，播放线程每 60 ms 检查一次、发现就停。
实测这避免了"播报完才连环响"的经典问题。

---

## 组成（workspace）

| crate | 产物 | 说明 |
|-------|------|------|
| `crates/core` | `star-trek-core` | 主后端：Agent 编排 + HTTP API + SSE + 拉起语音进程 |
| `crates/voice` | `voice-serve` | 独立语音进程：常驻麦克风采集 + KWS + ASR + TTS + 个人日志 |
| `crates/journal` | `journal` | 个人日志系统 skill 的 CLI（单二进制，走 voice-serve） |
| `crates/agents` | 库 `open-agent-sdk` | vendored 的 [open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust)（MIT），core 的 Agent 框架 |
| `crates/aggrsearch` | — | 独立搜索服务 crate，**不在 workspace 内**；已抽出为独立仓库发布 |

三个 Rust 二进制都不需要 Python 运行时。

搜索服务已抽出为独立项目，可单独使用：

```text
https://github.com/zhj-ldm/aggrsearch
```

多引擎并发（Bing 国际 / Bing 国内 / 百度 / 搜狗）、正文抓取、反爬退避、语义重排，
一个二进制同时提供 HTTP 服务、命令行搜索和网页正文抽取。

---

## 功能特性

- **语音闭环**（`voice-serve`）
  - 唤醒词 **KWS**（sherpa-onnx Zipformer，默认 `computer`）；整句打断词（默认 `computer stop`）
  - **ASR**：sherpa-onnx **Paraformer-zh**（FunASR 同源，替代 Whisper）
  - **TTS**：`edge-tts-rust`（内置），音色 / 语速可配，60 ms 级即时打断
  - 提示音；播报三重自保护（不自唤醒）；播报结束清缓冲
  - **常驻采集 + fan-out 广播**：唤醒、日志、一次性识别共用同一条 cpal 采集流
  - **唤醒诊断**：`/kws_diag` 最近 300 条（长度 / RMS / 噪声底 / 门限 / 增益 / 耗时 / 命中）
- **Agent**：主 Agent，工具含 `ImportSkill`、内置 `WebSearch`、可选 `AskUserQuestion`，
  其余 bash / 文件 / WebFetch 等由 SDK 默认注册
- **语音播报**：每轮结束由 core 用最终回复自动播报（取 `【Voice】` 段落，自动清洗 Markdown），
  **不再作为 AI 工具**
- **多上游分摊**：`providers` 支持多 Base URL / 多模型 / 多 API Key，round-robin 轮询并按上游独立 RPM 限流
- **Skills / 记忆 / 定时任务 / 多会话**：HTTP API + SSE 事件流全量支持
- **macOS 菜单栏常驻**：关窗后台运行、左键点击唤醒浮图、右键打开/退出
- **Electron UI**：对话 / 记忆 / Skills / 定时 / 设置（含唤醒日志面板）

---

## 快速开始

> ⚠️ **模型权重不随 Git 仓库分发**（ASR 约 234 MB，KWS 约 5 MB，`resources/models/**/*.onnx` 已在
> `.gitignore` 中）。**clone 之后必须先按下方「模型与依赖放置说明」准备模型，否则语音进程起不来。**
> 其余部分 clone 完直接编译即可。

```bash
# 1. 克隆（已含 vendored open-agent-sdk，clone 后即可编译）
git clone https://github.com/zhj-ldm/Star-Trek-voice-computer-rust.git
cd Star-Trek-voice-computer-rust

# 2. 编译后端（core + voice-serve + journal，无需 Python）
cargo build --release

# 3. 放置语音模型 —— 见下方「模型与依赖放置说明」，这步不能省

# 4. 安装 UI 依赖并启动（自动拉起后端 + 语音进程）
cd ui && npm install && cd ..
./run.sh
```

> 建议在**终端**运行 `./run.sh` 以继承 macOS 麦克风（TCC）授权；首次需在
> 「系统设置 → 隐私与安全性 → 麦克风」中允许终端访问麦克风。

---

## 模型与依赖放置说明

语音链路（KWS + ASR）需要 sherpa-onnx 模型，**仓库不内置模型权重**，仅保留词表 / 配置 / 下载脚本 / 测试音频。放置方式二选一：

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

**方式 B：环境变量指向外部目录**

```bash
export KWS_DIR=/path/to/kws-model-dir
export MODEL_DIR=/path/to/sherpa-onnx-paraformer-zh
export BEEP_FILE=/path/to/wake_sound.wav     # 可选
./run.sh
```

**编译依赖**：`voice-serve` 依赖 `sherpa-onnx` prebuilt 静态库。`SHERPA_ONNX_LIB_DIR` 环境变量优先；
未设置时构建脚本会自动下载对应平台 prebuilt。

> 关于"零 Python 依赖"的边界说明：模型**权重下载**可以借助 Python 生态里现成的脚本
> （例如 Paraformer 官方提供的 `download-model.py`），这一步是可选的一次性准备动作，
> 和项目的**运行时**无关。装完之后运行本项目不需要任何 Python 环境。

---

## 模型接入协议

程序通过 `open-agent-sdk` 调用大模型，**支持两种接入协议，二选一自动判定**：

| 协议 | 端点（由 Base URL 拼出） | 鉴权头 | 判定条件 |
|------|--------------------------|--------|----------|
| **OpenAI Chat Completions** | `POST {Base URL}/v1/chat/completions` | `Authorization: Bearer <key>` | 模型名命中：`gpt-` / `o1` / `o3` / `o4` / `deepseek` / `qwen` / `yi-` / `glm` / `mistral` / `gemma` / `mimo` / `llama` / `gemini` |
| **Anthropic Messages** | `POST {Base URL}/v1/messages` | `x-api-key` + `anthropic-version` | 以上都不命中时的**默认值** |

要点：

- **不是 OpenAI Responses API**（不使用 `/v1/responses`）。OpenAI 侧走标准的 **Chat Completions**。
- **斜杠与 `/v1` 写法**：Base URL 会先去掉末尾斜杠、再去掉末尾 `/v1`，然后统一拼 `/v1/...`。
  所以下面这些写法**等价、都能用**：
  - `https://api.openai.com` ✅
  - `https://api.openai.com/v1` ✅（推荐，最直观）
  - `https://api.openai.com/v1/` ✅
- **本地模型**（如 Ollama）填 `http://127.0.0.1:11434`（可省 `/v1`），本地地址自动走「关闭思考」等适配逻辑。
- **强制协议**：若模型名不在上面的启发式列表里，设环境变量
  `CODEANY_API_TYPE=openai-completions`（或 `anthropic-messages`）即可强制。
- 环境变量兜底：`CODEANY_API_KEY` / `CODEANY_BASE_URL` / `CODEANY_MODEL`。

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
│  └─ aggrsearch/                   独立搜索服务 crate（不在 workspace，已独立成仓库发布）
├─ Skills/                          运行在本程序之上的技能（见 Skills/README.md）
│  └─ personal-journal/             个人日志系统技能（SKILL.md + config.json + journal 二进制）
├─ ui/                              Electron 界面（package.json + electron/ + src/）
├─ resources/                       运行资源：音效 / prompts / models（模型权重不入库）
├─ data/                            运行时数据（不入库，含 API Key）
├─ run.sh                           一键启动
└─ update-app.sh                    编译并覆盖桌面 .app（含重签）
```

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
  "tts_backend": "internal",
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
- `kws_threshold` 是 KWS 判定阈值：调低更灵敏但更容易误触，调高更稳但可能漏唤醒。
  调之前建议先看 `/kws_diag` 的实际置信度分布。

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
| GET | `/api/events` | SSE 事件流（对话 / 工具 / 语音 / 记忆 / Skills / 定时） |
| POST | `/api/chat` · `/api/chat/interrupt` | 发送文本 / 打断 |
| POST | `/api/voice/wakeword` · `/api/voice/beep` · `/api/voice/listening` · `/api/voice/interrupt` | 语音回调 |
| GET/POST | `/api/config` | 读写配置 |
| GET/POST | `/api/skills` · `/api/skills/import` · `/api/skills/:name` · `/api/skills/:name/toggle` | Skills 管理 |
| GET/POST | `/api/memory` · `/api/memory/:id` · `/api/memory/clear` | 记忆 |
| GET/POST | `/api/schedules` · `/api/schedules/:id` | 定时任务（cron 5 字段） |
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
`Star Trek Computer.app`（含自签重签）。

打包后的 `.app` 支持菜单栏常驻：启动后自动驻留顶栏，关闭窗口后台继续运行，
左键点击唤出 Siri 风格唤醒浮图，右键提供「打开窗口 / 彻底退出」。

macOS 麦克风 TCC：如遇权限问题，用自签证书对 `.app` 深度重签。

---

## 许可证

本项目基于 [Apache-2.0](./LICENSE) 开源。vendored 的 `crates/agents`（open-agent-sdk-rust）为
[codeany-ai/open-agent-sdk-rust](https://github.com/codeany-ai/open-agent-sdk-rust) 的独立开源框架，遵循其自身 MIT 协议。