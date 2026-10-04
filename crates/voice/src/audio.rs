//! Continuous microphone capture with a 16kHz ring buffer for wake-word
//! detection, plus one-shot VAD recording for STT.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// cpal 0.15 on macOS coreaudio conservatively marks `cpal::Stream` as
/// !Send/!Sync (PhantomData<*mut ()>), even though the underlying stream is
/// an AudioObjectID with reference counting that is safe to move across
/// threads. Higher cpal versions already fix this. We keep the stream alive
/// through this wrapper so `AudioCapture` can live in axum state.
struct StreamKeep(#[allow(dead_code)] Option<cpal::Stream>);
unsafe impl Send for StreamKeep {}
unsafe impl Sync for StreamKeep {}

/// 二阶 Butterworth 低通，做抽取前的抗混叠滤波。
/// 旧实现是 44.1k 直接每 2.756 个点抽一个，8k~22k 的频率会折返回 0~8k 语音带，
/// 污染 KWS 置信度与能量估计。
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn lowpass(fs: f32, fc: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * fc / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos_w0) / 2.0) / a0,
            b1: (1.0 - cos_w0) / a0,
            b2: ((1.0 - cos_w0) / 2.0) / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// 相位连续的线性插值抽取器。
/// `pos` 跨回调保留——旧实现每个回调 `idx` 都从 0 重开，采样相位每秒跳变几十次，
/// 相当于给音频人为叠加时间轴抖动（唤醒/识别都受它影响）。
struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
    has_last: bool,
}

impl Resampler {
    fn new(ratio: f64) -> Self {
        Self {
            step: ratio.max(1.0),
            pos: 0.0,
            last: 0.0,
            has_last: false,
        }
    }

    fn process(&mut self, data: &[f32], out: &mut Vec<f32>) {
        let n = data.len() as f64;
        let mut i = self.pos;
        while i < n {
            let i0 = i.floor();
            let frac = (i - i0) as f32;
            let (a, b) = if i0 < 0.0 {
                if !self.has_last {
                    i += self.step;
                    continue;
                }
                (self.last, data.first().copied().unwrap_or(0.0))
            } else {
                let k = i0 as usize;
                (data[k], data.get(k + 1).copied().unwrap_or(data[k]))
            };
            out.push(a + (b - a) * frac);
            i += self.step;
        }
        self.pos = i - n;
        if let Some(l) = data.last() {
            self.last = *l;
            self.has_last = true;
        }
    }
}

/// 采集回调共享状态：16k 环形缓冲 + 抗混叠低通 + 重采样器 + 复用的临时缓冲
/// （临时缓冲复用，避免每个音频回调都分配内存）。
struct CapState {
    buffer: Vec<f32>,
    lp: Option<Biquad>,
    rs: Resampler,
    mono: Vec<f32>,
    out: Vec<f32>,
}

/// Keep the latest `window_secs` seconds of 16kHz mono f32 audio.
pub struct AudioCapture {
    state: Arc<Mutex<CapState>>,
    #[allow(dead_code)]
    window_secs: usize, // in samples @16k
    #[allow(dead_code)]
    running: AtomicBool,
    /// 是否收到过任何非零音频样本（区分「权限正常」与「全 0 静音（未授权）」）
    #[allow(dead_code)]
    alive: Arc<AtomicBool>,
    _stream: Option<StreamKeep>,
}

impl AudioCapture {
    /// 常驻采集。`alive` 由调用方传入（可跨重建共享），用于向 /status 汇报
    /// 「麦克风是否收到过真实音频」；重建采集（授权后恢复）时复用它，
    /// 保证 state.mic_alive 始终指向同一原子标志。
    pub fn new(
        window_secs: f32,
        alive: Arc<AtomicBool>,
        tx: Option<broadcast::Sender<Vec<f32>>>,
    ) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("未找到输入设备（麦克风）")?;
        let default = device
            .default_input_config()
            .context("读取默认输入配置失败")?;
        let input_rate = default.sample_rate().0 as f64;
        let channels = default.channels() as usize;
        let config: cpal::StreamConfig = cpal::StreamConfig {
            channels: default.channels(),
            sample_rate: default.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let window_samples = (16000.0 * window_secs) as usize;
        let ratio = input_rate / 16000.0;
        // 只有真的降采样时才需要抗混叠（截止 7k，留出过渡带）
        let lp = if ratio > 1.05 {
            Some(Biquad::lowpass(input_rate as f32, 7000.0))
        } else {
            None
        };
        let state: Arc<Mutex<CapState>> = Arc::new(Mutex::new(CapState {
            buffer: Vec::with_capacity(window_samples + 8192),
            lp,
            rs: Resampler::new(ratio),
            mono: Vec::new(),
            out: Vec::new(),
        }));

        let err_fn = |e| eprintln!("[audio error] {e}");
        let alive = alive.clone();
        let state_ref = state.clone();
        let alive_ref = alive.clone();
        let tx_ref = tx.clone();
        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _| {
                    if !alive_ref.load(Ordering::SeqCst)
                        && data.iter().any(|s| s.abs() > 1e-4)
                    {
                        // macOS 未授予麦克风权限时，回调仍在跑但数据恒为 0
                        alive_ref.store(true, Ordering::SeqCst);
                    }
                    let mut st = state_ref.lock().unwrap();
                    let CapState {
                        buffer,
                        lp,
                        rs,
                        mono,
                        out,
                    } = &mut *st;
                    mono.clear();
                    if channels <= 1 {
                        mono.extend_from_slice(data);
                    } else {
                        let mut i = 0;
                        while i + channels <= data.len() {
                            let sum: f32 = data[i..i + channels].iter().sum();
                            mono.push(sum / channels as f32);
                            i += channels;
                        }
                    }
                    if let Some(lp) = lp.as_mut() {
                        for s in mono.iter_mut() {
                            *s = lp.step(*s);
                        }
                    }
                    out.clear();
                    rs.process(mono, out);
                    buffer.extend_from_slice(out);
                    let len = buffer.len();
                    if len > window_samples {
                        buffer.drain(0..(len - window_samples));
                    }
                    // fan-out：把本次重采样后的 16k chunk 广播给订阅者。
                    // 仅在存在订阅者时克隆，避免无谓分配；send 非阻塞，不拖慢音频回调。
                    if let Some(tx) = &tx_ref {
                        if tx.receiver_count() > 0 {
                            let _ = tx.send(out.clone());
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("build input stream")?;
        stream.play().context("play stream")?;

        Ok(Self {
            state,
            window_secs: window_samples,
            running: AtomicBool::new(true),
            alive,
            _stream: Some(StreamKeep(Some(stream))),
        })
    }

    /// 是否收到过任何非零样本（false 表示极可能未授权麦克风）
    #[allow(dead_code)]
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub fn alive_ref(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    /// Latest ~N seconds of 16k mono samples (clamped to window).
    pub fn snapshot(&self) -> Vec<f32> {
        self.state.lock().unwrap().buffer.clone()
    }

    /// 清空采集缓冲。
    /// AI 播报（TTS 出声）刚结束时调用：扬声器的声音会被麦克风重新收回来，
    /// 残留在检测窗口里的"自己的声音"会被当成用户说话（AI 自己念到 computer
    /// 就把自己唤醒）。清空后必须重新积累 ≥400ms 新音频才会再判定。
    pub fn discard(&self) {
        self.state.lock().unwrap().buffer.clear();
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// Record one utterance with VAD auto-stop. Returns (sample_rate, samples).
/// （保留：/listen_once 已改为复用常驻采集，此处不再调用）
#[allow(dead_code)]
pub fn record_once(max_secs: Option<f64>) -> Result<(i32, Vec<f32>)> {
    use std::time::{Duration, Instant};

    const SILENCE_LIMIT_MS: u64 = 1200;
    const RMS_THRESHOLD: f32 = 0.008;
    const MIN_SECS: f64 = 0.4;
    const DEFAULT_MAX_SECS: f64 = 30.0;

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("未找到输入设备（麦克风）")?;
    let default = device.default_input_config().context("读取默认输入配置失败")?;
    let sample_rate = default.sample_rate().0;
    let config: cpal::StreamConfig = cpal::StreamConfig {
        channels: default.channels(),
        sample_rate: default.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let err_fn = |e| eprintln!("[audio error] {e}");
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let samples_cb = samples.clone();
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _| {
            if let Ok(mut v) = samples_cb.lock() {
                v.extend_from_slice(data);
            }
        },
        err_fn,
        None,
    )?;
    stream.play()?;

    let max_secs = max_secs.unwrap_or(DEFAULT_MAX_SECS);
    let start = Instant::now();
    let mut silent_ms: u64 = 0;
    let mut triggered = false;
    let mut last_len = 0usize;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let new = samples.lock().unwrap().len();
        if new > last_len {
            let seg: Vec<f32> = {
                let v = samples.lock().unwrap();
                v[last_len..new].to_vec()
            };
            last_len = new;
            let rms = seg.iter().map(|s| s * s).sum::<f32>() / seg.len().max(1) as f32;
            if rms.sqrt() > RMS_THRESHOLD {
                triggered = true;
                silent_ms = 0;
            } else if triggered {
                silent_ms += 50;
            }
        }
        if triggered && silent_ms >= SILENCE_LIMIT_MS {
            break;
        }
        if start.elapsed().as_secs_f64() > max_secs {
            break;
        }
    }
    drop(stream);
    let v = samples.lock().unwrap().clone();
    if !triggered || start.elapsed().as_secs_f64() < MIN_SECS {
        return Ok((sample_rate as i32, Vec::new()));
    }
    Ok((sample_rate as i32, v))
}
