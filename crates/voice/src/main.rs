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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::time::{sleep_until, Duration};

#[derive(Clone)]
struct AppState {
    detector: Arc<Mutex<Option<wakeword::WakewordDetector>>>,
    /// 独立打断词检测器（整句 "computer stop"）。与唤醒检测器分离：sherpa KWS
    /// 单检测器命中一个词就返回，把 "computer stop" 整句作为一个关键词条目单独检测，
    /// 连读时才能整句命中——不再需要先武装 computer 再等 stop 的窗口逻辑。
    detector_break: Arc<Mutex<Option<wakeword::WakewordDetector>>>,
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
    /// 唤醒词模型目录（阈值热更新时重建检测器用）
    kws_dir: Arc<String>,
    /// 唤醒词检测阈值（f32 位模式，热更新）
    kws_threshold: Arc<AtomicU32>,
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

/// 定位项目根：环境变量 STAR_TREK_ROOT（core 拉起/打包场景注入）> 当前可执行文件向上定位 > 工作目录
fn voice_root() -> PathBuf {
    if let Ok(p) = std::env::var("STAR_TREK_ROOT") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            // bin_dir = .../target/debug|release
            if matches!(
                bin_dir.file_name().and_then(|n| n.to_str()),
                Some("debug") | Some("release")
            ) {
                if let Some(target_dir) = bin_dir.parent() {
                    if target_dir.file_name().and_then(|n| n.to_str()) == Some("target") {
                        if let Some(root) = target_dir.parent() {
                            if root.join("Cargo.toml").exists() {
                                return root.to_path_buf();
                            }
                        }
                    }
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

    let root = voice_root();
    // HOME 兜底改通用值（不再硬编码用户名）
    let home = std::env::var("HOME").unwrap_or_default();
    // KWS / Paraformer 模型目录：环境变量优先；其次项目内 resources/models 相对放置；
    // 最后回退到 $HOME 下的通用位置（历史路径，换机后按 README 放置或设环境变量）。
    let kws_dir = std::env::var("KWS_DIR").unwrap_or_else(|_| {
        let cand = root.join("resources/models/kws");
        if cand.exists() {
            cand.to_string_lossy().into_owned()
        } else {
            format!("{home}/Projects/goose/ui/desktop/resources/models/kws")
        }
    });
    let model_dir = std::env::var("MODEL_DIR").unwrap_or_else(|_| {
        let cand = root.join("resources/models/paraformer-zh");
        if cand.exists() {
            cand.to_string_lossy().into_owned()
        } else {
            format!("{home}/Projects/funasr-cli/models/sherpa-onnx-paraformer-zh-2023-09-14")
        }
    });
    let beep_file = std::env::var("BEEP_FILE").unwrap_or_else(|_| {
        root.join("resources/wake_sound.wav").to_string_lossy().into_owned()
    });
    let default_voice =
        std::env::var("DEFAULT_VOICE").unwrap_or_else(|_| "zh-CN-XiaoxiaoNeural".to_string());
    let default_rate: f32 = std::env::var("DEFAULT_RATE")
        .unwrap_or_else(|_| "1.05".to_string())
        .parse()
        .unwrap_or(1.05);
    let default_threshold: f32 = std::env::var("KWS_THRESHOLD")
        .unwrap_or_else(|_| "0.15".to_string())
        .parse()
        .unwrap_or(0.15);

    let detector = match wakeword::WakewordDetector::new(&PathBuf::from(&kws_dir), default_threshold) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("KWS 唤醒词模型加载失败（唤醒功能禁用）: {e}");
            None
        }
    };
    // 流式 KWS：兼容旧的「前端采集送后端检测」接口（已不再使用，保留以兼容调用方）
    let kws_stream = match wakeword::StreamingKws::new(&PathBuf::from(&kws_dir), default_threshold) {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::warn!("流式 KWS 初始化失败（前端唤醒检测禁用）: {e}");
            None
        }
    };
    // 独立打断词检测器（整句 computer stop），与唤醒检测器并行
    let detector_break = match wakeword::WakewordDetector::new_with_keywords(
        &PathBuf::from(&kws_dir),
        "keywords_break.txt",
        default_threshold,
    ) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!("打断词检测器初始化失败（打断功能禁用）: {e}");
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
        match audio::AudioCapture::new(6.0, mic_alive.clone()) {
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
        detector_break: Arc::new(Mutex::new(detector_break)),
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
        kws_dir: Arc::new(kws_dir),
        kws_threshold: Arc::new(AtomicU32::new(default_threshold.to_bits())),
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
        .route("/kws_config", post(kws_config))
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
/// 查询 AI 是否忙碌：本机 TTS 播报中 或 core 主流程 working。
async fn core_is_busy(state: &AppState) -> bool {
    if state.player.is_speaking() {
        return true;
    }
    let status_url = format!(
        "{}/api/status",
        state.core_url.trim_end_matches("/api/voice/wakeword")
    );
    match reqwest::Client::new()
        .get(&status_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) => {
            if let Ok(v) = r.json::<serde_json::Value>().await {
                return v.get("main_status").and_then(|s| s.as_str()) == Some("working")
                    || v.get("speaking").and_then(|s| s.as_bool()).unwrap_or(false);
            }
        }
        Err(e) => tracing::warn!("查询 core 状态失败: {e}"),
    }
    false
}


/// 执行语音打断：停本机 TTS → 通知 core 中断生成 → 播打断提示音。
/// 唤醒路径与整句打断路径（computer stop）共用此方法。
async fn do_interrupt(state: &AppState) {
    // 1) 立即停止本机 TTS 播放
    state.player.interrupt();
    // 2) 通知 core 打断当前生成（幂等；core 内部也会再调 voice interrupt）
    let interrupt_url = format!(
        "{}/api/chat/interrupt",
        state.core_url.trim_end_matches("/api/voice/wakeword")
    );
    let client = reqwest::Client::new();
    match client
        .post(&interrupt_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(
            "通知 core 打断返回非成功状态: {} (url={})",
            resp.status(),
            interrupt_url
        ),
        Err(e) => tracing::warn!("通知 core 打断失败: {e}"),
    }
    // 3) 打断成功提示音（项目 resources 相对路径，跨平台可迁移）
    {
        let player = state.player.clone();
        let beep_path = voice_root()
            .join("resources/computerbeep_75.mp3")
            .to_string_lossy()
            .into_owned();
        tokio::spawn(async move {
            match player.play_file(&beep_path) {
                Ok(_) => {}
                Err(e) => tracing::warn!("播放打断提示音失败 {beep_path}: {e}"),
            }
        });
    }
}

async fn wakeword_loop(state: AppState) {
    // 离线实测（say 合成语音 + 本轮唤醒模型/关键词表）：
    //  · 单字 "computer"：1.2s / 1.8s / 2.4s / 3.0s 窗口全部稳定命中，
    //    音量降到 0.1x 仍命中 —— 唤醒词本身不弱，加长到 2.4s 是为了给流式
    //    zipformer（chunk-16-left-64）更多左上下文、并降低跨窗口边界的丢词概率。
    //  · 整句 "computer stop"（0.95s）：1.2s 窗口 100% 漏检、1.8s 只有一半命中，
    //    2.4s / 3.0s 才 100% 命中 —— 整句关键词必须配长窗口，这是硬约束。
    const DETECT_HZ: Duration = Duration::from_millis(300);
    const BUSY_HZ: Duration = Duration::from_millis(200);
    const WIN_SAMPLES: usize = 16000 * 24 / 10; // 2.4s @16k（唤醒窗口）
    const BREAK_WIN_SAMPLES: usize = 16000 * 3; // 3.0s @16k（整句 "computer stop"）
    // 只有"刚听到 computer"后的这段时间才运行打断检测器：用户此刻在补一句 stop，
    // 此时才需要更长窗口去整句匹配。平时不跑打断检测，省一半算力，
    // 且对用户完全不暴露任何"窗口"概念。
    const BREAK_ARM: Duration = Duration::from_millis(2500);
    // 命中后必须让"刚说出口的那句话"完全滑出检测窗口才能再判定：窗口 2.4s，
    // 故抑制 2.6s。旧实现在唤醒/打断后只冷却 1s，而窗口是 2.4s——同一句
    // "computer stop" 仍在窗口内，此时 AI 已被打断变空闲，残留的 computer 就被
    // 当成新唤醒（日志实测：每次"执行打断"后约 1.26s 必有一次空闲态唤醒命中）。
    const SUPPRESS: Duration = Duration::from_millis(2600);

    let mut cooldown_until = tokio::time::Instant::now();
    let mut wake_cooldown_until = tokio::time::Instant::now();
    let mut break_until = tokio::time::Instant::now();
    let mut tick = tokio::time::Instant::now();
    let mut diag = 0u32;
    // 噪声底：低于它就即时下探，否则每 tick 上抬 1%（约 35s 跟随环境）。
    // 旧实现上抬权重 0.0005（≈2000 tick 才收敛），环境一变就长期失配 —— 这正是
    // "不管改什么都一改就坏"的根源：门控与 AGC 互相耦合且跟随速度不匹配。
    let mut noise_floor: f32 = 0.001;
    let mut hit_prev = false;
    // 上一轮 TTS 是否正在出声（用于捕捉「播报刚结束」这一瞬间去清缓冲）
    let mut was_speaking = false;
    loop {
        // 节拍重锚：上一轮检测耗时若超过节拍，直接把 tick 对齐到现在，
        // 否则 sleep_until 会立即返回、循环退化成无休眠忙等（CPU 抖动进一步伤灵敏度）。
        let now0 = tokio::time::Instant::now();
        if tick < now0 {
            tick = now0;
        }
        let is_busy_now = state.busy.load(Ordering::SeqCst);
        tick += if is_busy_now { BUSY_HZ } else { DETECT_HZ };
        let until = tick;
        sleep_until(until).await;

        // ── AI 播报（TTS 出声）期间的自保护 ─────────────────────────────
        // 扬声器播放的声音会被麦克风原样收回来。若播报期间照常判定唤醒，AI 自己
        // 念到 "computer" 就会把自己唤醒（现象：播报结束后不久莫名又醒了一次）。
        // 处理：① 播报期间不做唤醒（只保留整句 "computer stop" 的打断能力）；
        //      ② 播报刚结束的那一瞬间清空采集缓冲，让残留在 2.4s 窗口里的
        //         "自己的声音"彻底滑出，而不是等它自己漂出窗口。
        let speaking_now = state.player.is_speaking();
        if was_speaking && !speaking_now {
            if let Some(c) = state.capture.lock().unwrap().as_ref() {
                c.discard();
            }
            tracing::info!("🔇 播报结束：已清空采集缓冲（防止把自己刚说的声音当成唤醒词）");
        }
        was_speaking = speaking_now;

        // 仅当监听总开关关闭时才跳过检测。
        // busy/播报中不再跳过：否则"工作中喊 Computer"永远检测不到（历史根因）。
        if !state.listening.load(Ordering::SeqCst) {
            continue;
        }
        // 全局 cooldown（唤醒/打断后防连读重复触发）
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
        if snap.len() < 6400 {
            continue;
        }
        let snap_len = snap.len();
        let win = if snap.len() > WIN_SAMPLES {
            snap[snap.len() - WIN_SAMPLES..].to_vec()
        } else {
            snap.clone()
        };
        let rms_sqrt =
            (win.iter().map(|s| s * s).sum::<f32>() / win.len().max(1) as f32).sqrt();

        // 自适应噪声底：即时下探 + 缓慢上抬
        if !hit_prev && !is_busy_now {
            if rms_sqrt < noise_floor {
                noise_floor = rms_sqrt;
            } else {
                noise_floor *= 1.01;
            }
        }
        // 静音门控：只为跳过纯静音，不做增益估计（增益交给下面的峰值归一化）
        let gate = (noise_floor * 3.0).max(0.0018);
        if rms_sqrt < gate {
            continue;
        }
        // 电平归一化：按整窗 RMS 拉到 0.08（小声说话也能到模型期望电平），上限 40x，
        // 再用峰值约束 0.95 保证绝不削顶（三者取最小）。旧实现只 clamp 到 ±1，
        // 遇到爆破音/瞬态会削顶失真、带来偶发漏检。
        let peak = win.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let gain = (0.08f32 / rms_sqrt).min(40.0).min(0.95 / peak.max(1e-6));
        let detect_win: Vec<f32> = win.iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect();

        let t0 = tokio::time::Instant::now();
        let keyword = {
            let det = state.detector.lock().unwrap();
            match det.as_ref() {
                Some(d) => d.detect(&detect_win),
                None => None,
            }
        };
        let now_i = tokio::time::Instant::now();
        let norm = keyword
            .as_deref()
            .map(|k| k.to_lowercase().replace('\u{2581}', "").replace(' ', ""));
        let is_wake = matches!(norm.as_deref(), Some("computer") | Some("heycomputer"));
        if is_wake {
            break_until = now_i + BREAK_ARM;
        }
        // 打断检测器（独立关键词表：整句 computer stop）。
        // sherpa KWS 单检测器命中第一个词就返回，所以 "computer stop" 必须由独立
        // 检测器 + 更长窗口做整句匹配，不能靠"先武装再等第二个词"的窗口逻辑。
        let break_keyword = if now_i < break_until {
            let bwin = if snap.len() > BREAK_WIN_SAMPLES {
                snap[snap.len() - BREAK_WIN_SAMPLES..].to_vec()
            } else {
                snap.clone()
            };
            let bpeak = bwin.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let brms =
                (bwin.iter().map(|s| s * s).sum::<f32>() / bwin.len().max(1) as f32).sqrt();
            let bgain = (0.08f32 / brms.max(1e-6)).min(40.0).min(0.95 / bpeak.max(1e-6));
            let bdet_win: Vec<f32> =
                bwin.iter().map(|s| (s * bgain).clamp(-1.0, 1.0)).collect();
            let detb = state.detector_break.lock().unwrap();
            match detb.as_ref() {
                Some(d) => d.detect(&bdet_win),
                None => None,
            }
        } else {
            None
        };
        let elapsed_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let hit = keyword.is_some() || break_keyword.is_some();
        hit_prev = hit;

        // 诊断：每 20 轮或命中时打一条，覆盖采集/门控/增益/耗时，便于直接定位根因
        diag += 1;
        if diag % 20 == 0 || hit {
            tracing::info!(
                "KWS: len={} rms={:.5} floor={:.5} gate={:.5} gain={:.2} ms={:.0} listening={} busy={} speaking={} hit={}",
                snap_len,
                rms_sqrt,
                noise_floor,
                gate,
                gain,
                elapsed_ms,
                state.listening.load(Ordering::SeqCst),
                is_busy_now,
                speaking_now,
                hit
            );
        }

        // 命中时才查询 AI 忙碌状态：决定 computer 是唤醒（空闲）还是等 stop（忙碌）
        let busy_now = if hit { core_is_busy(&state).await } else { false };

        if let Some(kw) = &keyword {
            tracing::info!("⚠️  唤醒词命中: {kw}");
            if tokio::time::Instant::now() < wake_cooldown_until {
                tracing::debug!("忽略重复 computer 命中（冷却中）");
            } else {
                wake_cooldown_until = tokio::time::Instant::now() + Duration::from_millis(1200);
                if is_wake {
                    if speaking_now {
                        // AI 正在出声时的 computer 判定为扬声器回授（自己的声音），
                        // 不唤醒。break_until 已在上面续期，所以仍可用整句
                        // "computer stop" 打断正在播报的 AI。
                        tracing::info!("🔇 播报中命中 computer：判定为自身声音回授，不唤醒");
                    } else if busy_now {
                        // AI 忙碌：单个 computer 不打断，2.5s 内补一句 stop 即打断
                        tracing::info!("🎙️  computer 命中且 AI 忙碌：2.5s 内听到 stop 即打断");
                    } else {
                        cooldown_until = tokio::time::Instant::now() + SUPPRESS;
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
                } else {
                    tracing::debug!("未知关键词命中，忽略: {kw}");
                }
            }
        }
        // 打断：命中整句 "computer stop" 且 AI 忙碌 → 立即打断
        if let Some(bkw) = &break_keyword {
            if busy_now {
                cooldown_until = tokio::time::Instant::now() + SUPPRESS;
                // 一并清掉"打断授权窗口"，避免残留的同一句话再触发一次打断
                break_until = tokio::time::Instant::now();
                tracing::info!(
                    "🎙️  整句打断词命中：{bkw} → 执行打断（{}ms 内不再判定，避免同一句再次唤醒）",
                    SUPPRESS.as_millis()
                );
                do_interrupt(&state).await;
            } else {
                tracing::debug!("{bkw} 命中但 AI 空闲：忽略（空闲只需 computer 唤醒）");
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

#[derive(Deserialize)]
struct KwsConfigReq {
    threshold: f32,
}

/// 热更新唤醒词检测阈值：更新原子值并重建检测器（模型加载约几百 ms，设置保存时一次性操作可接受）
async fn kws_config(State(st): State<AppState>, Json(req): Json<KwsConfigReq>) -> Json<TextResp> {
    let t = req.threshold.clamp(0.01, 1.0);
    st.kws_threshold
        .store(t.to_bits(), Ordering::SeqCst);
    let dir = st.kws_dir.to_string();
    let new_detector = wakeword::WakewordDetector::new(&PathBuf::from(&dir), t);
    let new_stream = wakeword::StreamingKws::new(&PathBuf::from(&dir), t);
    match (new_detector, new_stream) {
        (Ok(d), Ok(s)) => {
            *st.detector.lock().unwrap() = Some(d);
            *st.kws_stream.lock().unwrap() = Some(s);
            tracing::info!("KWS 阈值已更新为 {t}");
            Json(TextResp {
                text: "kws config updated".into(),
            })
        }
        (Err(e), _) | (_, Err(e)) => Json(TextResp {
            text: format!("error: {e}"),
        }),
    }
}

/// 重建常驻麦克风采集。渲染进程首次获得系统麦克风授权后调用：
/// macOS 在授权前 cpal 回调恒为静音（mic_alive=false），重建 stream 让其拿到真实音频。
async fn reinit_capture(State(st): State<AppState>) -> Json<TextResp> {
    let ok = {
        let mut cap = st.capture.lock().unwrap();
        // 复用 state.mic_alive 指向的同一原子标志，重建后 /status 依然能反映新采集状态
        *cap = match audio::AudioCapture::new(6.0, st.mic_alive.clone()) {
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
