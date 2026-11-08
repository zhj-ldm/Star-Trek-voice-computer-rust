# 语音前端设计深潜

> 本文是 Star Trek Computer 的语音采集、唤醒、VAD、ASR 与防自唤醒实现说明。
> 返回项目主页：[README](../README.md)

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
【唤醒】→ 提示音 → 开始录音
    ↓
Silero VAD push(&chunk)                此刻有没有人在说话
    ↓
连续 2.5 s 无语音 → 结束录音
    ↓
sherpa-onnx Paraformer-zh ASR          文字
```

注意这里有**两段判"有没有人说话"的逻辑，分属两个阶段**：
唤醒前靠 RMS 门控 + KWS 模型，唤醒后（录音何时结束）靠 Silero VAD。
下面两节分别讲。

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

### 录音什么时候结束：Silero VAD + 2.5 s 静默

唤醒之后进入录音，原先判"说完了没"用的是整段 RMS 能量对比一个固定阈值
（`RMS_THRESHOLD = 0.008`，连续 1.2 s 低于它就结束）。这个做法有两个实际毛病：

- **绝对阈值不跟着环境走**。换个安静的办公室，底噪就能长期超过 0.008，
  结果是永远判"还在说话"，一直录到 30 s 硬超时；换到吵的环境又把正常说话切断。
- **分不清人声和稳定噪声**。空调、冰箱、键盘声的能量是持续的，说话中间换气
  1.3 秒就会被判定成"说完了"，句子被从中间截断。

现在改用 [Silero VAD](https://github.com/snakers4/silero-vad)（629 KB onnx，
sherpa-onnx 内置，纯 ONNX Runtime 推理）判定"此刻有没有人在说话"：

```rust
// crates/voice/src/vad.rs
pub const SILENCE_LIMIT_SECS: f32 = 2.5;   // 说完后连续 2.5s 无人声 → 结束录音
pub const MIN_SPEECH_SECS: f32 = 0.4;      // 短于 0.4s 视为清嗓/敲键盘，不送去识别

config.silero_vad = SileroVadModelConfig {
    threshold: 0.5,              // 官方推荐工作点
    window_size: 512,            // Silero v5 固定值（16 kHz）
    min_speech_duration: 0.1,
    min_silence_duration: 0.2,   // 这两项只决定 detected() 翻转快慢
    max_speech_duration: 60.0,   // 必须 > 0，见下方「一个只看文档发现不了的坑」
    ..Default::default()
};

// crates/voice/src/main.rs —— listen_once 每收一块就问一次
let speaking = vad.push(&chunk);          // 有 VAD 模型时
if speaking { triggered = true; silent = 0; }
else if triggered { silent += chunk.len(); }
seg.extend_from_slice(&chunk);             // 音频照存，VAD 不改电平
if triggered && silent >= silence_limit { break; }
```

**2.5 s 这个数怎么定的**：太短会在句中换气、思考停顿处把话截断；
太长则说完话之后要干等很久才回应，体感迟钝。
2~3 s 是商用语音助手的常见区间，本项目取中间偏保守的值。

**模型缺失不阻断启动**：`Vad::new()` 返回 `Err` 时只打一条 warn，服务照常起，
录音结束判定自动退回原来的 RMS 逻辑。日志里会写明走的哪条路径：

```text
VAD 已加载（Silero）：录音结束阈值 2.5s 无语音
🎙️  录音 6.3s（VAD，结束 VadSilence）→ 识别结果: 北京今天天气怎么样
```

`GET /status` 的 `vad_ok` 字段可以确认当前是 VAD 还是降级状态。

#### 一个只看文档发现不了的坑

`sherpa-onnx` 的 C++ 层有个隐藏分支：

```cpp
// sherpa-onnx/csrc/voice-activity-detector.cc
if (buffer_.Size() > max_utterance_length_) {
    model_->SetThreshold(0.90);            // 激进模式
    model_->SetMinSilenceDuration(0.1);
} else { /* 用 config 里的 threshold / min_silence_duration */ }
```

而 `max_utterance_length_ = sample_rate * max_speech_duration`。
如果 `max_speech_duration = 0`，那么只要推入任何音频 `buffer_.Size() > 0` 就成立，
于是**永远走 threshold=0.90 的激进分支**，结果是安静环境下几乎检测不到人声。
所以这个值必须显式设成 > 0（本项目取 60 s）。Rust 绑定的文档里没写这一点，
只有读 C++ 源码才会发现。

### 关于 RMS 和 VAD 的关系（常见误解澄清）

网上不少方案会把 RMS 直接换成 Silero VAD 或 WebRTC VAD，理由是"更准"。
这两者解决的是**不同阶段**的问题：

| | RMS 门控（唤醒链路） | Silero VAD（录音链路） |
|---|---|---|
| 作用阶段 | 唤醒**之前**，持续监控 | 唤醒**之后**，决定录音何时结束 |
| 输出 | 标量能量值 | 人声概率 → 布尔判定 |
| 能改音频电平 | ✅ 参与增益计算 | ❌ 只输出判定 |
| 区分人声 / 噪声 | 粗（能量） | 细（模型） |
| 判定录音结束 | ❌ 只做静音跳过 | ✅ 主责 |

**它们是叠加关系，不是替代关系。**
VAD 能更准地分辨"是人声还是空调噪声"，但它**不改变音频本身的电平**——
而 Zipformer KWS 对输入电平是有期望的。所以本项目的做法是：

- **唤醒前**：RMS 负责电平归一化 + 静音门控，保证 KWS 每次拿到的输入电平稳定；
  "这句话是不是 computer" 交给 KWS 模型自己判断——它本来就是专门做这件事的。
- **唤醒后**：不再需要归一化，只需要"还有没有人说话"这一个布尔量，
  这正是 VAD 擅长的，且它不受环境底噪绝对值影响。

换句话说，**这里没有"自研 RMS 替代了 Silero VAD"这回事**，
能量前端、RMS 门控、VAD、识别模型各司其职。

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
