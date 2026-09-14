//! star-trek-core 主程序
//! 职责：加载配置 → 构建共享状态 → 启动定时任务循环 → 并行拉起 voice-serve
//! （语音链路独立进程）→ 启动 HTTP API。退出时终止子进程，UI 关闭即后端终止。

use star_core::config::{Config, DEFAULT_CORE_PORT};
use star_core::events::Event;
use star_core::http;
use star_core::scheduler::Scheduler;
use star_core::state::CoreState;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use tokio::sync::mpsc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    // ---------- 参数 ----------
    let mut data_dir_arg: Option<PathBuf> = None;
    let mut voice_bin_arg: Option<PathBuf> = None;
    let mut spawn_voice = true;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir_arg = args.get(i).map(PathBuf::from);
            }
            "--voice-bin" => {
                i += 1;
                voice_bin_arg = args.get(i).map(PathBuf::from);
            }
            "--no-spawn-voice" => spawn_voice = false,
            "--help" => {
                println!(
                    "star-trek-core [--data-dir <dir>] [--voice-bin <path>] [--no-spawn-voice]"
                );
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    // ---------- 配置 ----------
    let mut config = Config::default();
    if let Some(d) = &data_dir_arg {
        config.data_dir = d.clone();
    }
    std::fs::create_dir_all(&config.data_dir)?;
    let config_path = config.data_dir.join("config.json");
    config = Config::load(&config_path);
    if let Some(d) = &data_dir_arg {
        config.data_dir = d.clone();
    }

    let core_port: u16 = std::env::var("CORE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CORE_PORT);

    // ---------- 共享状态 ----------
    let mut core = CoreState::new(config.clone(), config_path.clone());

    // 语音客户端
    core.voice.set_base(config.voice_base());

    // 各子系统初始化
    core.skills.init(config.data_dir.join("skills-state.json"));
    core.skills.load(&config.skill_dirs).await;
    core.memory.init(config.data_dir.join("memory.json"));
    core.sessions.lock().await.init(config.data_dir.join("sessions.json"));

    // 定时任务：触发指令经 channel 交给主 Agent 执行
    let mut scheduler = Scheduler::default();
    scheduler.init(config.data_dir.join("schedules.json"));
    let (sched_tx, mut sched_rx) = mpsc::unbounded_channel::<(String, String)>();
    scheduler.set_on_trigger(Arc::new(move |id, prompt| {
        let _ = sched_tx.send((id, prompt));
    }));
    core.scheduler = scheduler;

    let core: Arc<CoreState> = Arc::new(core);

    // 定时任务触发消费
    {
        let core2 = core.clone();
        tokio::spawn(async move {
            while let Some((id, prompt)) = sched_rx.recv().await {
                core2.emit(Event::ScheduleTriggered {
                    id,
                    title: String::new(),
                });
                let _ = star_core::agents::run_main_turn(core2.clone(), prompt, None).await;
            }
        });
    }

    // 定时任务循环
    {
        let sched = core.scheduler.clone();
        tokio::spawn(async move {
            sched.run_loop().await;
        });
    }

    // ---------- 拉起 voice-serve（独立语音进程） ----------
    let mut voice_child: Option<Child> = None;
    if spawn_voice && config.voice_enabled {
        let bin = resolve_voice_bin(voice_bin_arg.clone());
        if let Some(bin) = bin {
            tracing::info!("spawning voice-serve: {}", bin.display());
            let callback = format!("http://127.0.0.1:{core_port}/api/voice/wakeword");
            let child = std::process::Command::new(&bin)
                .env("VOICE_PORT", config.voice_port.to_string())
                .env("CORE_CALLBACK_URL", callback)
                .env("BEEP_FILE", config.beep_file.clone())
                .env("DEFAULT_VOICE", config.voice.clone())
                .env("DEFAULT_RATE", config.rate.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(c) => voice_child = Some(c),
                Err(e) => tracing::warn!("voice-serve 启动失败: {e}"),
            }
        } else {
            tracing::warn!("未找到 voice-serve 二进制，语音功能不可用");
        }
    }

    // ---------- HTTP API ----------
    let app = http::router(core.clone());
    let addr = format!("127.0.0.1:{core_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("star-trek-core listening on http://{addr}");

    // 优雅退出：Ctrl-C / SIGTERM 时杀掉 voice-serve
    let child_ref = voice_child.as_mut();
    tokio::select! {
        _ = async {
            let _ = tokio::signal::ctrl_c().await;
        } => {}
        _ = axum::serve(listener, app) => {}
    }
    if let Some(c) = child_ref {
        let _ = c.kill();
        let _ = c.wait();
    }

    Ok(())
}

/// 定位 voice-serve 二进制：显式参数 > 当前可执行文件同目录 > workspace target
fn resolve_voice_bin(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in ["voice-serve", "../voice-serve", "../../voice-serve"] {
                let p = dir.join(cand);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/zhj".into());
    let p: PathBuf =
        format!("{home}/Projects/star-trek-assistant/target/debug/voice-serve").into();
    if p.exists() {
        return Some(p);
    }
    let p: PathBuf =
        format!("{home}/Projects/star-trek-assistant/target/release/voice-serve").into();
    if p.exists() {
        return Some(p);
    }
    None
}
