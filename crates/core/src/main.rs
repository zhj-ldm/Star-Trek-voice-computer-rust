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
    let mut parent_watch = true;
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
            "--no-parent-watch" => parent_watch = false,
            "--help" => {
                println!(
                    "star-trek-core [--data-dir <dir>] [--voice-bin <path>] [--no-spawn-voice] [--no-parent-watch]"
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
    // 路径迁移与解析：旧绝对路径（换机/构建产物重建后失效）→ 相对项目根 → 运行时绝对路径，
    // 并回写 config.json。缺失此接线会导致 beep_file / goose_tts_path 指向失效路径，
    // 表现为唤醒提示音不播报、AI 语音播报调用工具但无声（"No such file or directory"）。
    if config.migrate_absolute_paths()
        || !std::path::Path::new(&config.beep_file).is_absolute()
        || !std::path::Path::new(&config.goose_tts_path).is_absolute()
    {
        config.resolve_paths();
        let _ = config.save(&config_path);
    }
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
    // 技能目录 = 用户配置目录 + 内置技能目录（随 App 分发的 <项目根>/Skills）。
    // 内置目录始终参与扫描，保证随包分发的技能开箱可用；用户仍可在设置/UI 里禁用或导入外部技能。
    let mut skill_dirs = config.skill_dirs.clone();
    let builtin_skills = star_core::paths::project_root().join("Skills");
    if !skill_dirs.iter().any(|d| d == &builtin_skills) {
        skill_dirs.push(builtin_skills);
    }
    core.skills.load(&skill_dirs).await;
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
                let _ = star_core::agents::run_main_turn(core2.clone(), prompt, None, false).await;
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
        // 先清理残留的 voice-serve：上次 core 异常退出（SIGKILL/OOM）后，
        // voice 会成孤儿进程继续占着 VOICE_PORT，导致本次 spawn 绑定失败、语音链路静默断掉。
        // 必须用 -x 精确匹配进程名（不能用 -f 模糊匹配命令行，避免误杀无关进程）。
        let _ = std::process::Command::new("pkill")
            .arg("-9")
            .arg("-x")
            .arg("voice-serve")
            .output();
        let bin = resolve_voice_bin(voice_bin_arg.clone());
        if let Some(bin) = bin {
            tracing::info!("spawning voice-serve: {}", bin.display());
            let callback = format!("http://127.0.0.1:{core_port}/api/voice/wakeword");
            // voice-serve 的 tracing/错误输出落盘到 data_dir，便于诊断唤醒/提示音问题
            let voice_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(config.data_dir.join("voice-serve.log"));
            let (voice_stdout, voice_stderr) = match voice_file {
                Ok(f) => {
                    let out = f
                        .try_clone()
                        .map(std::process::Stdio::from)
                        .unwrap_or_else(|_| std::process::Stdio::null());
                    (out, std::process::Stdio::from(f))
                }
                Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
            };
            let child = std::process::Command::new(&bin)
                .env("VOICE_PORT", config.voice_port.to_string())
                .env("CORE_CALLBACK_URL", callback)
                .env("BEEP_FILE", config.beep_file.clone())
                .env("DEFAULT_VOICE", config.voice.clone())
                .env("DEFAULT_RATE", config.rate.to_string())
                .env("KWS_THRESHOLD", config.kws_threshold.to_string())
                .env(
                    "STAR_TREK_ROOT",
                    star_core::paths::project_root().to_string_lossy().into_owned(),
                )
                .stdout(voice_stdout)
                .stderr(voice_stderr)
                .spawn();
            match child {
                Ok(c) => voice_child = Some(c),
                Err(e) => tracing::warn!("voice-serve 启动失败: {e}"),
            }
        } else {
            tracing::warn!("未找到 voice-serve 二进制，语音功能不可用");
        }
    }

    // voice-serve 子进程句柄：优雅退出与父进程看门狗共享
    let voice_child = Arc::new(std::sync::Mutex::new(voice_child));

    // ---------- 父进程看门狗 ----------
    // Electron 退出（含强退/崩溃/被 kill）后，core 必须一起退出并带走 voice-serve，
    // 避免孤儿进程常驻端口。macOS/Linux 下父进程死亡会触发 reparent（ppid 变为 1/init），
    // 周期性比对父 PID 即可感知。App 正常退出时 SIGTERM 已覆盖，此看门狗兜底异常场景。
    if parent_watch {
        let parent_pid = unsafe { libc::getppid() };
        let voice_w = voice_child.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let now = unsafe { libc::getppid() };
                if now != parent_pid {
                    tracing::warn!(
                        "父进程已退出（ppid {parent_pid} -> {now}），终止 core 并连带 voice-serve"
                    );
                    if let Some(c) = voice_w.lock().unwrap().as_mut() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    std::process::exit(0);
                }
            }
        });
    }

    // ---------- 语音监听：启动后自动开启 ----------
    // 关掉窗口只剩菜单栏图标时，唤醒必须照常可用。而 voice-serve 的唤醒检测
    // 整体受 listening 总开关约束（关闭时整段 continue，computer / computer stop
    // 都不会响应），所以这里在 core 启动后自动开启监听。
    // voice-serve 加载 KWS/ASR 模型需要数秒，失败则重试，避免时序竞争。
    if config.voice_enabled {
        let core_auto = core.clone();
        tokio::spawn(async move {
            for i in 0..20 {
                if core_auto.voice.set_listening(true).await.is_ok() {
                    tracing::info!("🔊 语音监听已自动开启（窗口关闭后仍可唤醒）");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                if i == 19 {
                    tracing::warn!("自动开启语音监听失败：voice-serve 未就绪");
                }
            }
        });
    }

    // ---------- HTTP API ----------
    let app = http::router(core.clone());
    let addr = format!("127.0.0.1:{core_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("star-trek-core listening on http://{addr}");

    // 优雅退出：Ctrl-C / SIGTERM 时杀掉 voice-serve
    tokio::select! {
        _ = async {
            let _ = tokio::signal::ctrl_c().await;
        } => {}
        _ = axum::serve(listener, app) => {}
    }
    if let Some(c) = voice_child.lock().unwrap().as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }

    Ok(())
}

/// 定位 voice-serve 二进制：显式参数 > 当前可执行文件同目录 > workspace target（基于项目根相对解析）
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
    let root = star_core::paths::project_root();
    for profile in ["debug", "release"] {
        let p = root.join("target").join(profile).join("voice-serve");
        if p.exists() {
            return Some(p);
        }
    }
    None
}
