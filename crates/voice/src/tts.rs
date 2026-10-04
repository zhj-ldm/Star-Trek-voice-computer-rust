//! TTS synthesis (edge-tts-rust) + streaming playback (rodio + symphonia),
//! with interrupt support. Playback runs on a dedicated OS thread so the
//! Player handle stays Send + Sync for axum state. Adapted from the proven
//! goose voice-ext player.

use anyhow::{Context, Result};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

enum Cmd {
    Speak {
        text: String,
        voice: String,
        rate: f32,
        reply: Sender<bool>,
    },
    PlayFile {
        path: String,
        reply: Sender<bool>,
    },
    Interrupt,
}

/// Thread-safe handle to the background player thread.
#[derive(Clone)]
pub struct Player {
    tx: Sender<Cmd>,
    speaking: Arc<AtomicBool>,
    /// 即时打断标志（不经 mpsc 队列）：播放阻塞时 interrupt() 直接置位，
    /// 播放循环 60ms 内检查到立即停。避免打断命令排队导致"播报完才连环响"。
    interrupted: Arc<AtomicBool>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let (tx, rx) = channel::<Cmd>();
        let speaking = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let spk = speaking.clone();
        let intr = interrupted.clone();
        std::thread::Builder::new()
            .name("tts-player".into())
            .spawn(move || {
                match OutputStream::try_default() {
                    Ok((_stream, handle)) => player_loop(rx, handle, spk, intr),
                    Err(e) => eprintln!("[tts] open output stream failed: {e}"),
                }
            })
            .context("spawn tts player thread")?;
        Ok(Self { tx, speaking, interrupted })
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }

    pub fn interrupt(&self) {
        self.speaking.store(false, Ordering::SeqCst);
        // 关键：直接置位共享标志，播放线程正在播放时也能即时收到；
        // 不再依赖 mpsc 队列（播放阻塞时队列命令无法及时送达）。
        self.interrupted.store(true, Ordering::SeqCst);
    }

    /// Synthesize and play `text`, blocking until done or interrupted.
    /// Returns Ok(true) if played to completion, Ok(false) if interrupted.
    pub fn speak(&self, text: &str, voice: &str, rate: f32) -> Result<bool> {
        if text.trim().is_empty() {
            return Ok(true);
        }
        let (reply, rrx) = channel();
        self.tx.send(Cmd::Speak {
            text: text.to_string(),
            voice: voice.to_string(),
            rate,
            reply,
        })?;
        self.speaking.store(true, Ordering::SeqCst);
        let finished = rrx.recv().unwrap_or(true);
        self.speaking.store(false, Ordering::SeqCst);
        Ok(finished)
    }

    /// Play a local audio file (mp3/wav) once, blocking until done.
    pub fn play_file(&self, path: &str) -> Result<bool> {
        let (reply, rrx) = channel();
        self.tx.send(Cmd::PlayFile {
            path: path.to_string(),
            reply,
        })?;
        self.speaking.store(true, Ordering::SeqCst);
        let finished = rrx.recv().unwrap_or(true);
        self.speaking.store(false, Ordering::SeqCst);
        Ok(finished)
    }
}

fn player_loop(
    rx: Receiver<Cmd>,
    handle: OutputStreamHandle,
    speaking: Arc<AtomicBool>,
    interrupted: Arc<AtomicBool>,
) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Interrupt => {
                // 兼容旧路径：标志已由 Player::interrupt 直接置位，
                // 下一条 Speak/PlayFile 开头会复位。
            }
            Cmd::Speak {
                text,
                voice,
                rate,
                reply,
            } => {
                interrupted.store(false, Ordering::SeqCst);
                let finished = match edge_tts_synthesize(&text, &voice, rate) {
                    Ok(data) if !data.is_empty() => play_bytes(&handle, data, &interrupted),
                    _ => true,
                };
                let _ = reply.send(finished);
                let _ = speaking; // kept alive by handle
            }
            Cmd::PlayFile { path, reply } => {
                interrupted.store(false, Ordering::SeqCst);
                let finished = match std::fs::File::open(&path) {
                    Ok(file) => match Decoder::new(BufReader::new(file)) {
                        Ok(src) => play_source(&handle, Box::new(src), &interrupted),
                        Err(e) => {
                            eprintln!("[tts] decode file failed: {e}");
                            true
                        }
                    },
                    Err(e) => {
                        eprintln!("[tts] open file failed: {e}");
                        true
                    }
                };
                let _ = reply.send(finished);
            }
        }
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
    use edge_tts_rust::{EdgeTtsClient, SpeakOptions};
    let rate_pct = ((rate - 1.0) * 100.0).round() as i64;
    let rate_str = format!("{:+}%", rate_pct);
    let options = SpeakOptions {
        voice: voice.to_string(),
        rate: rate_str,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tts runtime")?;
    // EdgeTtsClient 必须在 tokio runtime 上下文中创建（内部需要 reactor），
    // 否则裸线程调用会 panic 打挂播放线程。整体放入 block_on。
    let result = rt
        .block_on(async {
            let client = EdgeTtsClient::new()
                .map_err(|e| anyhow::anyhow!("failed to build edge-tts-rust client: {e}"))?;
            client
                .synthesize(text.to_string(), options)
                .await
                .map_err(anyhow::Error::from)
        })
        .context("edge-tts-rust synthesize failed")?;
    Ok(result.audio)
}
