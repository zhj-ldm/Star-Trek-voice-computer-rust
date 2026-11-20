//! TTS synthesis (msedge-tts) + streaming playback (rodio + symphonia),
//! with interrupt support. Playback runs on a dedicated OS thread so the
//! Player handle stays Send + Sync for axum state.

use anyhow::{Context, Result};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// 合成网络请求超时：edge-tts 偶发挂起时不能把播放线程永久卡死
const SYNTH_TIMEOUT: Duration = Duration::from_secs(30);

enum Cmd {
    Speak {
        text: String,
        voice: String,
        rate: f32,
        /// 入队时的打断代次：小于当前代次说明已被 interrupt() 取消
        gen: u64,
        reply: Sender<std::result::Result<bool, String>>,
    },
    PlayFile {
        path: String,
        gen: u64,
        reply: Sender<std::result::Result<bool, String>>,
    },
}

/// Thread-safe handle to the background player thread.
#[derive(Clone)]
pub struct Player {
    tx: Sender<Cmd>,
    speaking: Arc<AtomicBool>,
    /// 即时打断标志（不经 mpsc 队列）：播放阻塞时 interrupt() 直接置位，
    /// 播放循环 60ms 内检查到立即停。避免打断命令排队导致"播报完才连环响"。
    interrupted: Arc<AtomicBool>,
    /// 打断代次：每次 interrupt() 自增。排队中的旧命令发现代次过期就直接丢弃，
    /// 否则打断后队列里剩下的语音仍会依次播完。
    gen: Arc<AtomicU64>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let (tx, rx) = channel::<Cmd>();
        let speaking = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let gen = Arc::new(AtomicU64::new(0));
        let spk = speaking.clone();
        let intr = interrupted.clone();
        let g = gen.clone();
        std::thread::Builder::new()
            .name("tts-player".into())
            .spawn(move || match OutputStream::try_default() {
                Ok((_stream, handle)) => player_loop(rx, handle, spk, intr, g),
                Err(e) => eprintln!("[tts] open output stream failed: {e}"),
            })
            .context("spawn tts player thread")?;
        Ok(Self {
            tx,
            speaking,
            interrupted,
            gen,
        })
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }

    pub fn interrupt(&self) {
        self.speaking.store(false, Ordering::SeqCst);
        // 关键：直接置位共享标志，播放线程正在播放时也能即时收到；
        // 不再依赖 mpsc 队列（播放阻塞时队列命令无法及时送达）。
        self.interrupted.store(true, Ordering::SeqCst);
        // 代次自增：已经排在队列里、尚未开始播放的命令会因代次过期被丢弃
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Synthesize and play `text`, blocking until done or interrupted.
    /// Ok(true) = 播完；Ok(false) = 被打断；Err = 合成/解码失败（上报给调用方）。
    pub fn speak(&self, text: &str, voice: &str, rate: f32) -> Result<bool> {
        if text.trim().is_empty() {
            return Ok(true);
        }
        let (reply, rrx) = channel();
        self.tx.send(Cmd::Speak {
            text: text.to_string(),
            voice: voice.to_string(),
            rate,
            gen: self.gen.load(Ordering::SeqCst),
            reply,
        })?;
        // speaking 标志由播放线程统一管理（命令开始置位/结束复位），
        // 避免调用方与 interrupt() 交错写标志产生"永远在播报"的竞态。
        match rrx.recv() {
            Ok(Ok(finished)) => Ok(finished),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Ok(true),
        }
    }

    /// Play a local audio file (mp3/wav) once, blocking until done.
    pub fn play_file(&self, path: &str) -> Result<bool> {
        let (reply, rrx) = channel();
        self.tx.send(Cmd::PlayFile {
            path: path.to_string(),
            gen: self.gen.load(Ordering::SeqCst),
            reply,
        })?;
        match rrx.recv() {
            Ok(Ok(finished)) => Ok(finished),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Ok(true),
        }
    }
}

fn player_loop(
    rx: Receiver<Cmd>,
    handle: OutputStreamHandle,
    speaking: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
    gen: Arc<AtomicU64>,
) {
    while let Ok(cmd) = rx.recv() {
        // 代次过期 = 入队后已被 interrupt() 取消：直接丢弃，不再播放
        let cmd_gen = match &cmd {
            Cmd::Speak { gen, .. } | Cmd::PlayFile { gen, .. } => *gen,
        };
        if cmd_gen < gen.load(Ordering::SeqCst) {
            let _ = match cmd {
                Cmd::Speak { reply, .. } | Cmd::PlayFile { reply, .. } => reply.send(Ok(false)),
            };
            continue;
        }
        // 每条命令开始时复位打断标志并置位播报状态：
        // 同一线程内先复位再置位，interrupt() 在命令执行期间置位不会被吞掉。
        interrupted.store(false, Ordering::SeqCst);
        speaking.store(true, Ordering::SeqCst);
        let result = match cmd {
            Cmd::Speak {
                text,
                voice,
                rate,
                reply,
                ..
            } => {
                let r = match edge_tts_synthesize(&text, &voice, rate) {
                    Ok(data) if !data.is_empty() => Ok(play_bytes(&handle, data, &interrupted)),
                    Ok(_) => Err("synthesize returned empty audio".to_string()),
                    Err(e) => {
                        eprintln!("[tts] synthesize failed: {e:#}");
                        Err(format!("{e:#}"))
                    }
                };
                let _ = reply.send(r.clone());
                r
            }
            Cmd::PlayFile { path, reply, .. } => {
                let r = match std::fs::File::open(&path) {
                    Ok(file) => match Decoder::new(BufReader::new(file)) {
                        Ok(src) => Ok(play_source(&handle, Box::new(src), &interrupted)),
                        Err(e) => Err(format!("decode file failed: {e}")),
                    },
                    Err(e) => Err(format!("open file failed: {e}")),
                };
                if let Err(e) = &r {
                    eprintln!("[tts] {e}");
                }
                let _ = reply.send(r.clone());
                r
            }
        };
        let _ = result;
        speaking.store(false, Ordering::SeqCst);
    }
}

fn play_bytes(handle: &OutputStreamHandle, data: Vec<u8>, interrupted: &Arc<AtomicBool>) -> bool {
    let cursor = std::io::Cursor::new(data);
    match Decoder::new(BufReader::new(cursor)) {
        Ok(src) => play_source(handle, Box::new(src), interrupted),
        Err(e) => {
            eprintln!("[tts] decode mp3 failed: {e}");
            true
        }
    }
}

fn play_source(
    handle: &OutputStreamHandle,
    source: Box<dyn rodio::Source<Item = i16> + Send>,
    interrupted: &Arc<AtomicBool>,
) -> bool {
    match Sink::try_new(handle) {
        Ok(sink) => {
            sink.append(source);
            sink.play();
            while !sink.empty() {
                if interrupted.load(Ordering::SeqCst) {
                    sink.stop();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
            true
        }
        Err(e) => {
            eprintln!("[tts] create sink failed: {e}");
            true
        }
    }
}

fn edge_tts_synthesize(text: &str, voice: &str, rate: f32) -> Result<Vec<u8>> {
    use msedge_tts::tts::client::tokio_runtime::connect_async;
    use msedge_tts::tts::SpeechConfig;
    let rate_pct = ((rate - 1.0) * 100.0).round() as i32;
    let config = SpeechConfig {
        voice_name: voice.to_string(),
        audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
        pitch: 0,
        rate: rate_pct,
        volume: 0,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tts runtime")?;
    // 客户端必须在 tokio runtime 上下文中创建（内部需要 reactor），
    // 否则裸线程调用会 panic 打挂播放线程。整体放入 block_on。
    let result = rt
        .block_on(async {
            tokio::time::timeout(SYNTH_TIMEOUT, async {
                let mut client = connect_async()
                    .await
                    .map_err(|e| anyhow::anyhow!("connect failed: {e}"))?;
                client
                    .synthesize(text, &config)
                    .await
                    .map_err(|e| anyhow::anyhow!("synthesize failed: {e}"))
            })
            .await
            .map_err(|_| anyhow::anyhow!("synthesize timed out after {}s", SYNTH_TIMEOUT.as_secs()))?
        })
        .context("edge tts synthesize failed")?;
    Ok(result.audio_bytes)
}