//! Continuous microphone capture with a 16kHz ring buffer for wake-word
//! detection, plus one-shot VAD recording for STT.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// cpal 0.15 on macOS coreaudio conservatively marks `cpal::Stream` as
/// !Send/!Sync (PhantomData<*mut ()>), even though the underlying stream is
/// an AudioObjectID with reference counting that is safe to move across
/// threads. Higher cpal versions already fix this. We keep the stream alive
/// through this wrapper so `AudioCapture` can live in axum state.
struct StreamKeep(Option<cpal::Stream>);
unsafe impl Send for StreamKeep {}
unsafe impl Sync for StreamKeep {}

/// Keep the latest `window_secs` seconds of 16kHz mono f32 audio.
pub struct AudioCapture {
    buffer: Arc<Mutex<Vec<f32>>>,
    window_secs: usize, // in samples @16k
    running: AtomicBool,
    /// 是否收到过任何非零音频样本（区分「权限正常」与「全 0 静音（未授权）」）
    alive: Arc<AtomicBool>,
    _stream: Option<StreamKeep>,
}

impl AudioCapture {
    /// 常驻采集。`alive` 由调用方传入（可跨重建共享），用于向 /status 汇报
    /// 「麦克风是否收到过真实音频」；重建采集（授权后恢复）时复用它，
    /// 保证 state.mic_alive 始终指向同一原子标志。
    pub fn new(window_secs: f32, alive: Arc<AtomicBool>) -> Result<Self> {
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

        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let window_samples = (16000.0 * window_secs) as usize;
        let err_fn = |e| eprintln!("[audio error] {e}");
        let alive = alive.clone();

        // Downsample from device rate to 16k (linear).
        let buffer_ref = buffer.clone();
        let alive_ref = alive.clone();
        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _| {
                    if !alive_ref.load(Ordering::SeqCst) {
                        // macOS 未授予麦克风权限时，回调仍在跑但数据恒为 0
                        if data.iter().any(|s| s.abs() > 1e-4) {
                            alive_ref.store(true, Ordering::SeqCst);
                        }
                    }
                    let mut v = buffer_ref.lock().unwrap();
                    let step = input_rate / 16000.0;
                    let mut idx = 0.0f64;
                    while (idx as usize) < data.len() && channels > 0 {
                        // average channels
                        let start = (idx as usize / channels) * channels;
                        if start + channels <= data.len() {
                            let sum: f32 = data[start..start + channels].iter().sum();
                            v.push(sum / channels as f32);
                        }
                        idx += step;
                    }
                    let len = v.len();
                    if len > window_samples {
                        let drop = len - window_samples;
                        v.drain(0..drop);
                    }
                },
                err_fn,
                None,
            )
            .context("build input stream")?;
        stream.play().context("play stream")?;

        Ok(Self {
            buffer,
            window_secs: window_samples,
            running: AtomicBool::new(true),
            alive,
            _stream: Some(StreamKeep(Some(stream))),
        })
    }

    /// 是否收到过任何非零样本（false 表示极可能未授权麦克风）
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn alive_ref(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    /// Latest ~N seconds of 16k mono samples (clamped to window).
    pub fn snapshot(&self) -> Vec<f32> {
        let v = self.buffer.lock().unwrap();
        v.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// Record one utterance with VAD auto-stop. Returns (sample_rate, samples).
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
