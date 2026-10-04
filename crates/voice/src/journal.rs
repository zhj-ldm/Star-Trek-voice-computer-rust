//! 个人日志系统：共用常驻麦克风采集（audio_tx fan-out），边录边落盘。
//!
//! 一次会话（开启 → 结束）对应 **一个音频文件**：
//!   · 只在有人声时才写入 wav（静音不落盘），整段会话的语音累积进同一个文件；
//!   · 采集回调与写盘解耦，转写走独立通道，互不阻塞，避免内存缓冲丢帧（截断）；
//!   · 后台按语音片段转写，结果按时间戳追加到与音频同名的 md。
//!
//! 与唤醒/打断/一次性识别共用同一条 cpal 采集流，不会出现第二路麦克风占用。

use anyhow::Result;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::stt::Stt;

const SAMPLE_RATE: u32 = 16000;
const RMS_THRESHOLD: f32 = 0.008; // 有无人声的门限
const SILENCE_LIMIT_SAMPLES: usize = 16000 * 1200 / 1000; // 停顿 1.2s 视为一句话结束（仅用于切转写，不切文件）
const PREROLL_SAMPLES: usize = 16000 / 3; // 句首保留 0.33s 前导，避免吞掉起音
const MAX_BURST_SAMPLES: usize = 16000 * 60; // 单句超过 60s 先转写一次，控制内存
const MIN_BURST_SAMPLES: usize = 16000 / 5; // 短于 0.2s 的碎音丢弃（不转写）

#[derive(Default)]
pub struct Stats {
    pub segments: AtomicU64,
    pub last_text: Mutex<String>,
    pub last_file: Mutex<String>,
    pub session_file: Mutex<String>,
}

pub struct Journal {
    running: Arc<AtomicBool>,
    root: Arc<Mutex<PathBuf>>,
    audio_tx: Arc<broadcast::Sender<Vec<f32>>>,
    stt: Arc<Mutex<Option<Stt>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stats: Arc<Stats>,
}

impl Journal {
    pub fn new(
        root: PathBuf,
        audio_tx: Arc<broadcast::Sender<Vec<f32>>>,
        stt: Arc<Mutex<Option<Stt>>>,
    ) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            root: Arc::new(Mutex::new(root)),
            audio_tx,
            stt,
            task: Mutex::new(None),
            stats: Arc::new(Stats::default()),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn root(&self) -> PathBuf {
        self.root.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn set_root(&self, root: PathBuf) {
        if let Ok(mut r) = self.root.lock() {
            *r = root;
        }
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// 开启日志：先等上一轮会话收尾，再开启新会话（幂等）
    pub async fn start(&self) -> Result<()> {
        let prev = self.task.lock().unwrap().take();
        if let Some(h) = prev {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
        self.running.store(true, Ordering::SeqCst);
        let handle = tokio::spawn(session_loop(
            self.running.clone(),
            self.root.clone(),
            self.audio_tx.clone(),
            self.stt.clone(),
            self.stats.clone(),
        ));
        *self.task.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// 结束日志：停止写入并等待本场收尾（含待转写队列落盘）
    pub async fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        let prev = self.task.lock().unwrap().take();
        if let Some(h) = prev {
            let _ = tokio::time::timeout(Duration::from_secs(30), h).await;
        }
        Ok(())
    }
}

/// 增量写 16k 单声道 PCM16 RIFF wav：边写边周期回填头部长度，
/// 即使中途异常退出，已落盘部分仍是一个可播放的合法 wav。
struct WavSink {
    file: File,
    data_bytes: u64,
    buf: Vec<u8>,
}

impl WavSink {
    fn create(path: &Path) -> Result<Self> {
        let mut file = File::create(path)?;
        file.write_all(&wav_header(0))?;
        Ok(Self {
            file,
            data_bytes: 0,
            buf: Vec::with_capacity(1 << 16),
        })
    }

    fn write(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        if self.buf.len() >= (1 << 16) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let _ = self.file.seek(SeekFrom::End(0));
        if self.file.write_all(&self.buf).is_ok() {
            self.data_bytes += self.buf.len() as u64;
        }
        self.buf.clear();
        self.patch();
    }

    fn patch(&mut self) {
        let h = wav_header(self.data_bytes);
        let _ = self.file.seek(SeekFrom::Start(4));
        let _ = self.file.write_all(&h[4..8]);
        let _ = self.file.seek(SeekFrom::Start(40));
        let _ = self.file.write_all(&h[40..44]);
    }

    fn finalize(&mut self) {
        self.flush();
        let _ = self.file.flush();
    }
}

fn wav_header(data_bytes: u64) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&(data_bytes as u32).to_le_bytes());
    h
}

async fn session_loop(
    running: Arc<AtomicBool>,
    root: Arc<Mutex<PathBuf>>,
    audio_tx: Arc<broadcast::Sender<Vec<f32>>>,
    stt: Arc<Mutex<Option<Stt>>>,
    stats: Arc<Stats>,
) {
    let root_now = root.lock().map(|r| r.clone()).unwrap_or_default();
    if root_now.as_os_str().is_empty() {
        tracing::warn!("日志根目录为空，会话未启动");
        running.store(false, Ordering::SeqCst);
        return;
    }
    let now = chrono::Local::now();
    let day = now.format("%Y-%m-%d").to_string();
    let stamp = now.format("%H-%M-%S").to_string();
    let audio_dir = root_now.join(&day).join("audio");
    let text_dir = root_now.join(&day).join("text");
    if let Err(e) =
        std::fs::create_dir_all(&audio_dir).and_then(|_| std::fs::create_dir_all(&text_dir))
    {
        tracing::warn!("创建日志目录失败 {:?}: {e}", root_now);
        running.store(false, Ordering::SeqCst);
        return;
    }
    let (wav_path, md_path) = unique_paths(&audio_dir, &text_dir, &stamp);
    let mut sink = match WavSink::create(&wav_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("创建日志音频失败 {:?}: {e}", wav_path);
            running.store(false, Ordering::SeqCst);
            return;
        }
    };
    let _ = std::fs::write(&md_path, format!("# {day} {stamp}\n\n"));
    if let Ok(mut f) = stats.session_file.lock() {
        *f = wav_path.to_string_lossy().into_owned();
    }
    if let Ok(mut f) = stats.last_file.lock() {
        *f = md_path.to_string_lossy().into_owned();
    }
    tracing::info!("📓 日志会话开始: {}", wav_path.display());

    // 转写独立通道：与采集/写盘解耦，队列无界，转写再慢也不会让主循环丢音频。
    let (tx, mut burst_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
    let worker = {
        let md_path = md_path.clone();
        let stt = stt.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut md = std::fs::OpenOptions::new().append(true).open(&md_path).ok();
            while let Some(burst) = burst_rx.recv().await {
                let stt2 = stt.clone();
                let text = tokio::task::spawn_blocking(move || {
                    let g = stt2.lock().unwrap();
                    match g.as_ref() {
                        Some(s) => s.transcribe(SAMPLE_RATE as i32, &burst).unwrap_or_default(),
                        None => String::new(),
                    }
                })
                .await
                .unwrap_or_default();
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let line = format!(
                    "## {}\n\n{}\n\n",
                    chrono::Local::now().format("%H:%M:%S"),
                    text
                );
                if let Some(f) = md.as_mut() {
                    let _ = f.write_all(line.as_bytes());
                }
                if let Ok(mut t) = stats.last_text.lock() {
                    *t = text;
                }
                stats.segments.fetch_add(1, Ordering::SeqCst);
            }
        })
    };

    let mut rx = audio_tx.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pre: Vec<f32> = Vec::new(); // 句前静音前导（未触发时滚动保留）
    let mut trail: Vec<f32> = Vec::new(); // 句尾静音（确认是尾巴则丢弃，接话则并入）
    let mut burst: Vec<f32> = Vec::new(); // 当前句（用于转写）
    let mut triggered = false;
    let mut silent = 0usize;

    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(chunk) => {
                    let rms = (chunk.iter().map(|s| s * s).sum::<f32>()
                        / chunk.len().max(1) as f32)
                        .sqrt();
                    if rms > RMS_THRESHOLD {
                        if !triggered {
                            triggered = true;
                            if !pre.is_empty() {
                                sink.write(&pre);
                                burst.extend_from_slice(&pre);
                                pre.clear();
                            }
                        }
                        if !trail.is_empty() {
                            // 句内短暂停顿后又开口：把这段停顿并入同一文件
                            sink.write(&trail);
                            burst.extend_from_slice(&trail);
                            trail.clear();
                        }
                        sink.write(&chunk);
                        burst.extend_from_slice(&chunk);
                        silent = 0;
                        if burst.len() >= MAX_BURST_SAMPLES {
                            let _ = tx.send(std::mem::take(&mut burst));
                        }
                    } else if triggered {
                        // 候选句尾：先攒着，确认是收尾就丢弃（静音不落盘）
                        trail.extend_from_slice(&chunk);
                        silent += chunk.len();
                        if silent >= SILENCE_LIMIT_SAMPLES {
                            trail.clear();
                            if burst.len() >= MIN_BURST_SAMPLES {
                                let _ = tx.send(std::mem::take(&mut burst));
                            } else {
                                burst.clear();
                            }
                            triggered = false;
                            silent = 0;
                        }
                    } else {
                        pre.extend_from_slice(&chunk);
                        if pre.len() > PREROLL_SAMPLES {
                            let d = pre.len() - PREROLL_SAMPLES;
                            pre.drain(0..d);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                sink.flush(); // 周期落盘（回填头部长度）
                if !running.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }

    // 收尾：把当前未完成的一句也送去转写，然后等队列跑完
    if triggered && burst.len() >= MIN_BURST_SAMPLES {
        let _ = tx.send(std::mem::take(&mut burst));
    }
    drop(tx);
    sink.finalize();
    let _ = worker.await;

    // 整场没有任何语音：删掉空 wav 与只有标题的 md
    if sink.data_bytes == 0 {
        let _ = std::fs::remove_file(&wav_path);
        let _ = std::fs::remove_file(&md_path);
        tracing::info!("📓 日志会话无语音，已清理空文件");
    } else {
        tracing::info!(
            "📓 日志会话结束: {} ({} 字节)",
            wav_path.display(),
            sink.data_bytes
        );
    }
    if let Ok(mut f) = stats.session_file.lock() {
        f.clear();
    }
    running.store(false, Ordering::SeqCst);
}

/// 同名冲突时追加 -2/-3…，避免同一秒内两场互相覆盖
fn unique_paths(audio_dir: &Path, text_dir: &Path, stamp: &str) -> (PathBuf, PathBuf) {
    let mut i = 1;
    loop {
        let suffix = if i == 1 { String::new() } else { format!("-{i}") };
        let wav = audio_dir.join(format!("{stamp}{suffix}.wav"));
        let md = text_dir.join(format!("{stamp}{suffix}.md"));
        if !wav.exists() && !md.exists() {
            return (wav, md);
        }
        i += 1;
    }
}
