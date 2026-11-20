//! star-trek-core 主程序
//! 职责：加载配置 → 构建共享状态 → 启动定时任务循环 → 并行拉起 voice-serve
//! （语音链路独立进程）→ 启动 HTTP API。退出时终止子进程，UI 关闭即后端终止。

use star_core::agents;
use star_core::config::{self, Config, DEFAULT_CORE_PORT};
use star_core::events::Event;
use star_core::http;
use star_core::scheduler::Scheduler;
use star_core::state::CoreState;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use tokio::sync::mpsc;

fn main() -> anyhow::Result<()> {
    // 剔除非法 UTF-8 环境变量：下游 Bash 工具用 std::env::vars() 继承环境，
    // 遇到损坏的键/值（如 shell 传入的 PWD/_）会 unwrap panic 使工具调用卡死。
    // 必须在创建 tokio 运行时（多线程）之前做：多线程下 unsetenv 不是线程安全的。
    let bad_env: Vec<std::ffi::OsString> = std::env::vars_os()
        .filter(|(k, v)| k.to_str().is_none() || v.to_str().is_none())
        .map(|(k, _)| k)
        .collect();
    for k in bad_env {
        std::env::remove_var(&k);
    }

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    // rpi（reqwest + rustls）与语音链路可能各自链接 TLS 后端：显式安装 ring provider，
    // 避免进程内 CryptoProvider 未选择导致的 panic。
    let _ = rustls::crypto::ring::default_provider().install_default();
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
    // 配置迁移，两类历史遗留都要在这一步收敛：
    //  1) 枚举值：旧外部 TTS 后端（"goose-tts"）已移除该链路，统一收敛为内置 edge-tts；
    //     不迁移会让 TTS 选择停留在一个已不存在的后端上。
    //  2) 路径：旧绝对路径（换机/构建产物重建后失效）→ 相对项目根 → 运行时绝对路径；
    //     缺失此接线会导致 beep_file 指向失效路径，表现为唤醒提示音不播报
    //     （"No such file or directory"）。
    //  3) 废弃键：旧配置里的已删除字段（如外部 TTS 可执行文件路径）被 serde 静默忽略，
    //     只有整体回写一次 config.json，它们才会真正从磁盘消失。
    let has_legacy_keys = config::has_legacy_keys(&config_path);
    let values_changed = config.migrate_legacy_values();
    let paths_changed = config.migrate_absolute_paths();
    if values_changed || paths_changed || has_legacy_keys {
        config.resolve_paths();
        let _ = config.save(&config_path);
    } else if !std::path::Path::new(&config.beep_file).is_absolute() {
        // 未触发回写也要保证运行期拿到绝对路径
        config.resolve_paths();
    }
    if let Some(d) = &data_dir_arg {
        config.data_dir = d.clone();
    }
    // 上次若在录音中被强杀，系统输出会停留在静音状态；启动时先恢复。
    agents::init_mute_state(&config.data_dir);

    // 内嵌向量模型：优先使用安装包内随附的 EmbeddingGemma 缓存，首次搜索无需联网下载。
    // 模型以 HuggingFace 缓存目录结构放在 resources/models/aggrsearch-models 下。
    let bundled_embed = star_core::paths::project_root().join("resources/models/aggrsearch-models");
    if bundled_embed.is_dir() && std::env::var_os("AGGRSEARCH_EMBED_CACHE").is_none() {
        std::env::set_var("AGGRSEARCH_EMBED_CACHE", &bundled_embed);
    }
    // 预热放后台线程：int8 模型首次加载需要数秒，避免第一次联网搜索被拖慢。
    tokio::task::spawn_blocking(aggrsearch::embedding::warmup);

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
    core.sessions
        .lock()
        .await
        .init(config.data_dir.join("sessions.json"));

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
                // 用户正在对话时不打断：等主 Agent 空闲再执行（最多等 10 分钟）
                let mut waited_busy = true;
                for _ in 0..1200 {
                    if !core2.main_busy.load(std::sync::atomic::Ordering::SeqCst) {
                        waited_busy = false;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                if waited_busy {
                    tracing::warn!("定时任务 {id} 等待 10 分钟后主 Agent 仍忙碌，本次跳过");
                    continue;
                }
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
                    star_core::paths::project_root()
                        .to_string_lossy()
                        .into_owned(),
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
                    agents::restore_if_muted();
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

    // MCP 预热：启动即连接已启用的 MCP 服务并缓存工具清单，
    // 首轮对话就能直接调用 MCP 工具，前端连接器页也立即有真实状态。
    {
        let core_mcp = core.clone();
        tokio::spawn(async move {
            let servers = core_mcp.mcp.list().await;
            for s in servers.iter().filter(|s| s.enabled) {
                let st = core_mcp.mcp.probe(s).await;
                if !st.connected {
                    tracing::warn!("MCP「{}」启动连接失败: {:?}", s.name, st.error);
                }
            }
            core_mcp.emit(star_core::events::Event::McpUpdated);
        });
    }

    // ---------- HTTP API ----------
    let app = http::router(core.clone());
    let addr = format!("127.0.0.1:{core_port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("star-trek-core listening on http://{addr}");

    // 优雅退出：Ctrl-C / SIGTERM 时杀掉 voice-serve。
    // Electron 关闭走 SIGTERM——不接住它会让 core 被直接杀死，
    // 静音恢复与 voice-serve 清理全部落空。
    let shutdown = async {
        #[cfg(unix)]
        {
            if let Ok(mut sig) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sig.recv() => {}
                }
                return;
            }
        }
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::select! {
        _ = shutdown => {}
        _ = axum::serve(listener, app) => {}
    }
    // 正常退出前恢复输出，避免录着音退出时把系统留在静音。
    agents::restore_if_muted();
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
