//! voice-serve —— 星际迷航语音链路独立进程
//! 常驻：cpal 麦克风采集 + KWS 唤醒词监听 + STT(Paraformer) + TTS(EdgeTTS) + 提示音播放。
//! 采集完全在纯 Rust 后端完成（cpal），渲染进程（Electron）不采集音频、只做 UI 展示。
//! 通过 HTTP 供 core 调用；唤醒词命中后回调 core。

mod audio;
mod stt;
mod tts;
mod wakeword;

use anyhow::Result;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::time::{sleep_until, Duration};

#[derive(Clone)]
struct AppState {
    detector: Arc<Mutex<Option<wakeword::WakewordDetector>>>,
    kws_stream: Arc<Mutex<Option<wakeword::StreamingKws>>>,
    stt: Arc<Mutex<Option<stt::Stt>>>,
    player: Arc<tts::Player>,
    capture: Arc<Mutex<Option<audio::AudioCapture>>>,
    listening: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    mic_alive: Arc<AtomicBool>,
    core_url: Arc<String>,
    voice: Arc<String>,
    rate: Arc<Mutex<f32>>,
    beep_file: Arc<String>,
    /// 常驻唤醒循环的诊断日志（前端「唤醒日志」面板数据源）
    kws_diag: Arc<Mutex<VecDeque<KwsDiagEntry>>>,
}

/// 单条唤醒检测诊断：时间戳 / 窗口样本数 / RMS / 检测耗时 / 是否命中
#[derive(Serialize, Clone)]
struct KwsDiagEntry {
    ts: u64,
    samples: usize,
    win_len: usize,
    rms: f32,
    elapsed_ms: f32,
    hit: bool,
    keyword: Option<String>,
}

#[derive(Deserialize)]
struct SpeakReq {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    rate: Option<f32>,
}

#[derive(Deserialize)]
struct BeepReq {
    #[serde(default)]
    file: Option<String>,
}

#[derive(Deserialize)]
struct ListenReq {
    #[serde(default)]
    max_secs: Option<f64>,
}

#[derive(Serialize)]
struct TextResp {
    text: String,
}

#[derive(Serialize)]
struct KwResp {
    keyword: Option<String>,
}

#[derive(Deserialize)]
struct ListeningReq {
    enabled: bool,
}

/// （已废弃，兼容保留）前端采集音频检测请求：audio 为 base64 编码的 16-bit little-endian PCM。
/// 新架构采集完全在后端（cpal），前端不再送音频，此接口仅向后兼容历史调用方。
#[derive(Deserialize)]
struct AudioDetectReq {
    audio: String,
    #[serde(default = "default_sr")]
    sample_rate: i32,
}

fn default_sr() -> i32 {
    16000
}

/// base64 PCM16 -> 归一化 f32 采样（[-1, 1]）
fn pcm16_to_f32(b64: &str) -> Result<Vec<f32>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
    Ok(bytes
        .chunks_exact(2)
        .map(|c| {
            let s = i16::from_le_bytes([c[0], c[1]]);
            (s as f32) / 32768.0
        })
        .collect())
}

#[derive(Serialize)]
struct StatusResp {
    listening: bool,
    speaking: bool,
    busy: bool,
    voice: String,
    /// 麦克风是否收到过真实音频数据（false = 疑似未授权）
    mic_alive: bool,
    /// 常驻采集是否可用
    cap_ok: bool,
    /// 唤醒词模型是否加载成功
    kws_ok: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let port: u16 = std::env::var("VOICE_PORT")
        .unwrap_or_else(|_| "8420".to_string())
        .parse()
        .unwrap_or(8420);
    let core_url = std::env::var("CORE_CALLBACK_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8410/voice/wakeword".to_string());

    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/zhj".into());
    let kws_dir = std::env::var("KWS_DIR").unwrap_or_else(|_| {
        format!("{home}/Projects/goose/ui/desktop/resources/models/kws")
    });
    let model_dir = std::env::var("MODEL_DIR").unwrap_or_else(|_| {
        format!("{home}/Projects/funasr-cli/models/sherpa-onnx-paraformer-zh-2023-09-14")
    });
    let beep_file = std::env::var("BEEP_FILE").unwrap_or_else(|_| {
        format!("{home}/Projects/star-trek-assistant/resources/wake_sound.wav")
    });
    let default_voice =
        std::env::var("DEFAULT_VOICE").unwrap_or_else(|_| "zh-CN-XiaoxiaoNeural".to_string());
    let default_rate: f32 = std::env::var("DEFAULT_RATE")
        .unwrap_or_else(|_| "1.05".to_string())
        .parse()
        .unwrap_or(1.05);

    let detector = match wakeword::WakewordDetector::new(&PathBuf::from(&kws_dir)) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("KWS 唤醒词模型加载失败（唤醒功能禁用）: {e}");
            None
        }
    };
    // 流式 KWS：兼容旧的「前端采集送后端检测」接口（已不再使用，保留以兼容调用方）
    let kws_stream = match wakeword::StreamingKws::new(&PathBuf::from(&kws_dir)) {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::warn!("流式 KWS 初始化失败（前端唤醒检测禁用）: {e}");
            None
        }
    };
    let stt = match stt::Stt::new(&PathBuf::from(&model_dir)) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!("STT 模型加载失败: {e}");
            return Err(anyhow::anyhow!("STT model load failed: {e}"));
        }
    };
    let player = Arc::new(tts::Player::new()?);
    let mic_alive = Arc::new(AtomicBool::new(false));
    let capture = if detector.is_some() {
        // alive 标志由外部传入，授权后重建采集时继续复用（见 /reinit_capture）
        match audio::AudioCapture::new(3.0, mic_alive.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("常驻监听采集启动失败: {e}");
                None
            }
        }
    } else {
        None
    };

    let state = AppState {
        detector: Arc::new(Mutex::new(detector)),
        kws_stream: Arc::new(Mutex::new(kws_stream)),
        stt: Arc::new(Mutex::new(stt)),
        player,
        capture: Arc::new(Mutex::new(capture)),
        listening: Arc::new(AtomicBool::new(false)),
        busy: Arc::new(AtomicBool::new(false)),
        mic_alive,
        core_url: Arc::new(core_url),
        voice: Arc::new(default_voice),
        rate: Arc::new(Mutex::new(default_rate)),
        beep_file: Arc::new(beep_file),
        kws_diag: Arc::new(Mutex::new(VecDeque::new())),
    };

    // 常驻唤醒词监听循环
    let st = state.clone();
    tokio::spawn(async move {
        wakeword_loop(st).await;
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/speak", post(speak))
        .route("/interrupt", post(interrupt))
        .route("/is_speaking", get(is_speaking))
        .route("/beep", post(beep))
        .route("/listen_once", post(listen_once))
        .route("/wakeword_once", post(wakeword_once))
        .route("/wakeword_detect", post(wakeword_detect))
        .route("/kws_reset", post(kws_reset))
        .route("/kws_diag", get(kws_diag))
        .route("/transcribe", post(transcribe))
        .route("/listening", post(set_listening))
        .route("/reinit_capture", post(reinit_capture))
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("voice-serve listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------- wakeword 常驻监听 ----------

/// 常驻唤醒词监听：低频（1s/次）+ 最新 1.2s 窗口推理，避免 CPU 打满。
async fn wakeword_loop(state: AppState) {
    const DETECT_HZ: Duration = Duration::from_millis(1000);
    const WIN_SAMPLES: usize = 16000 * 12 / 10; // 1.2s @16k
    let mut cooldown_until = tokio::time::Instant::now();
    let mut tick = tokio::time::Instant::now();
    let mut diag = 0u32;
    loop {
        // 固定节拍，防止 detect 耗时把循环挤成忙等
        tick += DETECT_HZ;
        let until = tick;
        sleep_until(until).await;

        // 需要常驻监听、有采集、模型可用、且当前不在说话/录音
        if !state.listening.load(Ordering::SeqCst)
            || state.busy.load(Ordering::SeqCst)
            || state.player.is_speaking()
        {
            continue;
        }
        if tokio::time::Instant::now() < cooldown_until {
            continue;
        }
        let snap = {
            let cap = state.capture.lock().unwrap();
            match cap.as_ref() {
                Some(c) => c.snapshot(),
                None => continue,
            }
        };
        // 至少 0.4 秒音频，且只取最新 1.2s 窗口做检测
        if snap.len() < 6400 {
            continue;
        }
        let snap_len = snap.len();
        let win = if snap.len() > WIN_SAMPLES {
            snap[snap.len() - WIN_SAMPLES..].to_vec()
        } else {
            snap
        };
        // 静音门控：窗口能量过低（纯静音）直接跳过推理，省 CPU 又保持唤醒灵敏度
        let rms = win.iter().map(|s| s * s).sum::<f32>() / win.len().max(1) as f32;
        let rms_sqrt = rms.sqrt();
        // 诊断：每 10s 打印一次采集与能量状态，便于定位"没发唤醒"根因
        diag += 1;
        if diag % 10 == 0 {
            tracing::info!(
                "KWS 诊断: snap_len={} win_len={} rms={:.5} listening={} busy={} cap_ok={}",
                snap_len,
                win.len(),
                rms_sqrt,
                state.listening.load(Ordering::SeqCst),
                state.busy.load(Ordering::SeqCst),
                state.capture.lock().unwrap().is_some()
            );
        }
        if rms_sqrt < 0.002 {
            continue;
        }
        let t0 = tokio::time::Instant::now();
        let keyword = {
            let det = state.detector.lock().unwrap();
            match det.as_ref() {
                Some(d) => d.detect(&win),
                None => None,
            }
        };
        let elapsed_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let hit = keyword.is_some();
        if let Some(kw) = &keyword {
            tracing::info!("⚠️  唤醒词命中: {kw}");
            cooldown_until = tokio::time::Instant::now() + Duration::from_secs(3);
            // 回调 core
            let body = serde_json::json!({"keyword": kw});
            let client = reqwest::Client::new();
            match client
                .post(state.core_url.as_str())
                .json(&body)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => tracing::warn!(
                    "回调 core 返回非成功状态: {} (url={})",
                    resp.status(),
                    state.core_url.as_str()
                ),
                Err(e) => tracing::warn!("回调 core 失败: {e}"),
            }
        }
        // 写入诊断日志（前端「唤醒日志」面板；新记录在前）
        let ts = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        {
            let mut d = state.kws_diag.lock().unwrap();
            d.push_front(KwsDiagEntry {
                ts,
                samples: snap_len,
                win_len: win.len(),
                rms: rms_sqrt,
                elapsed_ms,
                hit,
                keyword,
            });
            if d.len() > 200 {
                d.truncate(200);
            }
        }
    }
}

// ---------- handlers ----------

async fn health() -> &'static str {
    "voice-serve ok"
}

async fn status(State(st): State<AppState>) -> Json<StatusResp> {
    let cap_ok = st.capture.lock().map(|c| c.is_some()).unwrap_or(false);
    let kws_ok = st.detector.lock().map(|d| d.is_some()).unwrap_or(false);
    Json(StatusResp {
        listening: st.listening.load(Ordering::SeqCst),
        speaking: st.player.is_speaking(),
        busy: st.busy.load(Ordering::SeqCst),
        voice: st.voice.to_string(),
        mic_alive: st.mic_alive.load(Ordering::SeqCst),
        cap_ok,
        kws_ok,
    })
}

async fn speak(State(st): State<AppState>, Json(req): Json<SpeakReq>) -> Json<TextResp> {
    st.busy.store(true, Ordering::SeqCst);
    let voice = req
        .voice
        .clone()
        .unwrap_or_else(|| st.voice.to_string());
    let rate = req.rate.unwrap_or_else(|| *st.rate.lock().unwrap());
    let player = st.player.clone();
    let result = tokio::task::spawn_blocking(move || player.speak(&req.text, &voice, rate))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
    st.busy.store(false, Ordering::SeqCst);
    match result {
        Ok(_) => Json(TextResp {
            text: "ok".into(),
        }),
        Err(e) => Json(TextResp {
            text: format!("error: {e}"),
        }),
    }
}

async fn interrupt(State(st): State<AppState>) -> Json<TextResp> {
    st.player.interrupt();
    Json(TextResp {
        text: "interrupted".into(),
    })
}

async fn is_speaking(State(st): State<AppState>) -> Json<TextResp> {
    Json(TextResp {
        text: if st.player.is_speaking() {
            "true".into()
        } else {
            "false".into()
        },
    })
}

async fn beep(State(st): State<AppState>, Json(req): Json<BeepReq>) -> Json<TextResp> {
    let file = req
        .file
        .unwrap_or_else(|| st.beep_file.to_string());
    st.busy.store(true, Ordering::SeqCst);
    let player = st.player.clone();
    let r = tokio::task::spawn_blocking(move || player.play_file(&file))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
    st.busy.store(false, Ordering::SeqCst);
    Json(TextResp {
        text: r.map(|_| "ok".into()).unwrap_or_else(|e| format!("error: {e}")),
    })
}

async fn listen_once(
    State(st): State<AppState>,
    Json(req): Json<ListenReq>,
) -> Json<TextResp> {
    st.busy.store(true, Ordering::SeqCst);
    let (sr, samples) = match audio::record_once(req.max_secs) {
        Ok(x) => x,
        Err(e) => {
            st.busy.store(false, Ordering::SeqCst);
            return Json(TextResp {
                text: format!("error: {e}"),
            });
        }
    };
    let stt_guard = st.stt.lock().unwrap();
    let text = match stt_guard.as_ref() {
        Some(s) => s.transcribe(sr, &samples).unwrap_or_default(),
        None => String::new(),
    };
    drop(stt_guard);
    st.busy.store(false, Ordering::SeqCst);
    tracing::info!("🎙️  识别结果: {text}");
    Json(TextResp { text })
}

/// （已废弃，兼容保留）前端采集音频唤醒词检测：Electron 渲染进程 Web Audio 采集的 PCM16
/// 直接送后端 KWS，命中返回 keyword。新架构由常驻 wakeword_loop 完成，此接口不再被前端调用。
/// 使用流式检测器：增量音频滑窗解码，命中后自动重建。
async fn wakeword_detect(
    State(st): State<AppState>,
    Json(req): Json<AudioDetectReq>,
) -> Json<KwResp> {
    let samples = match pcm16_to_f32(&req.audio) {
        Ok(s) if !s.is_empty() => s,
        _ => return Json(KwResp { keyword: None }),
    };
    let kw = {
        let mut det = st.kws_stream.lock().unwrap();
        det.as_mut().and_then(|d| d.feed(&samples))
    };
    Json(KwResp { keyword: kw })
}

/// 重置流式唤醒检测器（前端开启监听 / 切换会话时调用）
async fn kws_reset(State(st): State<AppState>) -> Json<TextResp> {
    st.kws_stream
        .lock()
        .map(|mut d| {
            if let Some(d) = d.as_mut() {
                d.reset();
            }
        })
        .unwrap_or(());
    Json(TextResp {
        text: "kws reset".into(),
    })
}

/// 唤醒检测诊断日志（前端「唤醒日志」面板，数据来自常驻循环）
async fn kws_diag(State(st): State<AppState>) -> Json<serde_json::Value> {
    let diag: Vec<KwsDiagEntry> = st.kws_diag.lock().unwrap().iter().cloned().collect();
    Json(serde_json::to_value(diag).unwrap_or(serde_json::json!([])))
}

/// 重建常驻麦克风采集。渲染进程首次获得系统麦克风授权后调用：
/// macOS 在授权前 cpal 回调恒为静音（mic_alive=false），重建 stream 让其拿到真实音频。
async fn reinit_capture(State(st): State<AppState>) -> Json<TextResp> {
    let ok = {
        let mut cap = st.capture.lock().unwrap();
        // 复用 state.mic_alive 指向的同一原子标志，重建后 /status 依然能反映新采集状态
        *cap = match audio::AudioCapture::new(3.0, st.mic_alive.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("重建常驻采集失败: {e}");
                None
            }
        };
        cap.is_some()
    };
    st.mic_alive.store(false, Ordering::SeqCst);
    Json(TextResp {
        text: if ok {
            "capture reinitialized".into()
        } else {
            "error: capture init failed".into()
        },
    })
}

/// （已废弃，兼容保留）前端采集音频转写：PCM16 -> Paraformer STT。新架构由
/// core 调用 /listen_once 完成后端采集→STT，此接口不再被前端调用。
async fn transcribe(
    State(st): State<AppState>,
    Json(req): Json<AudioDetectReq>,
) -> Json<TextResp> {
    let samples = match pcm16_to_f32(&req.audio) {
        Ok(s) if !s.is_empty() => s,
        _ => return Json(TextResp { text: "error: bad audio".into() }),
    };
    let sr = req.sample_rate.clamp(8000, 48000);
    let stt_guard = st.stt.lock().unwrap();
    let text = match stt_guard.as_ref() {
        Some(s) => s.transcribe(sr, &samples).unwrap_or_default(),
        None => String::new(),
    };
    drop(stt_guard);
    tracing::info!("🎙️  识别结果: {text}");
    Json(TextResp { text })
}

async fn wakeword_once(
    State(st): State<AppState>,
    Json(req): Json<ListenReq>,
) -> Json<KwResp> {
    // 用常驻采集做一次性唤醒监听（若不可用则返回 None）
    let cap = {
        let c = st.capture.lock().unwrap();
        c.as_ref().map(|c| c.snapshot())
    };
    let kw = match cap {
        Some(snap) => {
            let det = st.detector.lock().unwrap();
            det.as_ref().and_then(|d| d.detect(&snap))
        }
        None => None,
    };
    let _ = req;
    Json(KwResp { keyword: kw })
}

async fn set_listening(
    State(st): State<AppState>,
    Json(req): Json<ListeningReq>,
) -> Json<TextResp> {
    st.listening.store(req.enabled, Ordering::SeqCst);
    Json(TextResp {
        text: if req.enabled {
            "listening on".into()
        } else {
            "listening off".into()
        },
    })
}
