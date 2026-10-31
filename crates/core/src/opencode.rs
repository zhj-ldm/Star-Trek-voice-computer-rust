//! OpenCode CLI 分支：模型配置的 base_url 填 `opencode` 时，
//! 本轮指令不再走内置 Agent，而是交给本机已安装的 opencode-cli 子进程执行。
//! CLI 以 `--format json` 逐行输出事件流，这里解析并映射成项目自身的 SSE 事件，
//! 让前端复用已有的「文本快照 / 工具卡 / 完成」渲染。

use crate::events::Event;
use crate::sessions::ToolCallRecord;
use crate::state::CoreState;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

/// base_url 为这些值即视为「走本机 OpenCode CLI」
const OPENCODE_BASE: [&str; 3] = ["opencode", "opencode://", "opencode://cli"];

pub struct OpenCodeTarget {
    /// CLI 可执行文件路径
    pub cli: String,
    /// 传给 -m 的完整模型名（provider/model）
    pub model: String,
    /// 专属提示词（新会话首条消息前注入）
    pub prompt: String,
}

/// app 会话内续接的 OpenCode 会话：记录 session id 与所用模型。
/// 模型变了就另起一个新会话，避免旧会话继续用旧模型回答。
struct OcSession {
    id: String,
    model: String,
}

fn oc_sessions() -> &'static Mutex<HashMap<String, OcSession>> {
    static MAP: OnceLock<Mutex<HashMap<String, OcSession>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 当前生效模型是否指向 OpenCode CLI；是则返回本轮所需的目标信息。
pub async fn active_target(core: &Arc<CoreState>) -> Option<OpenCodeTarget> {
    let cfg = core.config.lock().await;
    if cfg.providers.is_empty() {
        return None;
    }
    let active = cfg
        .providers
        .iter()
        .find(|p| !cfg.active_model.is_empty() && p.name == cfg.active_model)
        .or_else(|| cfg.providers.first())?;
    let base = active.base_url.trim().to_ascii_lowercase();
    if !OPENCODE_BASE.iter().any(|b| base == *b) {
        return None;
    }

    // CLI 路径：配置存在则用之，否则回退 PATH 里的 opencode-cli
    let cli = resolve_cli(&cfg.opencode_cli_path);

    // 模型名：允许只填 big-pickle，自动补 opencode/ 前缀（CLI 要求 provider/model）
    let raw = active.model.trim();
    let model = if raw.is_empty() {
        "opencode/big-pickle".to_string()
    } else if raw.contains('/') {
        raw.to_string()
    } else {
        format!("opencode/{raw}")
    };

    Some(OpenCodeTarget {
        cli,
        model,
        prompt: cfg.opencode_prompt.clone(),
    })
}

/// 解析 CLI 路径：配置的路径不存在时回退 PATH 中的 opencode-cli
pub fn resolve_cli(configured: &str) -> String {
    let p = configured.trim();
    if !p.is_empty() && std::path::Path::new(p).exists() {
        p.to_string()
    } else {
        "opencode-cli".to_string()
    }
}

/// 预热后台服务：OpenCode 的 run/models 默认复用同一后台服务，
/// 提前拉起可省掉首轮的启动开销（服务已存在时该命令立即返回）。
pub fn warm_service(cli: &str) {
    let cli = cli.to_string();
    tokio::spawn(async move {
        let _ = Command::new(&cli)
            .args(["service", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    });
}

/// 列出 OpenCode 能识别的全部模型（形如 `opencode/big-pickle`）。
/// 走后台服务，正常几十毫秒返回。
pub async fn list_models(cli: &str) -> Result<Vec<String>, String> {
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(cli)
            .arg("models")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| "列举模型超时（OpenCode 后台服务可能未启动）".to_string())?
    .map_err(|e| format!("无法运行 OpenCode CLI：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("OpenCode 列举模型失败：{}", err.trim()));
    }
    let mut ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && l.contains('/'))
        .map(|l| l.to_string())
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// 跑一轮 OpenCode：负责事件映射、持久化、结束通知与自动播报。
/// 进入前 run_main_turn 已完成 busy/turn_session/status 置位与用户消息落盘。
pub async fn run_turn(
    core: Arc<CoreState>,
    sid: String,
    user_text: String,
    target: OpenCodeTarget,
    t0: std::time::Instant,
) {
    let prev = {
        let map = oc_sessions().lock().await;
        map.get(&sid)
            .filter(|s| s.model == target.model)
            .map(|s| s.id.clone())
    };

    // 续接已有会话时不重复注入提示词；新会话首条消息前注入专属提示词
    let mut message = String::new();
    if prev.is_none() && !target.prompt.trim().is_empty() {
        message.push_str(target.prompt.trim());
        message.push_str("\n\n");
    }
    message.push_str(&user_text);

    let mut cmd = Command::new(&target.cli);
    // 不传 --standalone：复用 OpenCode 的后台服务（已由启动时预热），
    // 省掉每轮拉起私有 server 的开销；服务不存在时 CLI 会自行启动。
    cmd.arg("run")
        .arg("--format")
        .arg("json")
        .arg("-m")
        .arg(&target.model)
        // 自动批准工具权限：语音场景无人值守，不能卡在权限确认上
        .arg("--auto");
    if let Some(s) = &prev {
        cmd.arg("-s").arg(s);
    }
    cmd.arg(&message);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let text = format!(
                "OpenCode CLI 启动失败（{}）：{e}\n请在设置中检查 OpenCode CLI 路径",
                target.cli
            );
            persist_and_done(&core, &sid, &text, Vec::new(), t0).await;
            core.main_busy.store(false, Ordering::SeqCst);
            *core.turn_session.lock().await = None;
            core.set_main_status("idle");
            return;
        }
    };

    // stderr 单独收集：CLI 出错时用于回报原因
    let stderr_pipe = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(err) = stderr_pipe {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                buf.push_str(&l);
                buf.push('\n');
            }
        }
        buf
    });

    let mut final_text = String::new();
    let mut tool_log: Vec<ToolCallRecord> = Vec::new();
    let mut new_session: Option<String> = None;
    let mut interrupted = false;

    // stdout 必须在独立任务里持续读取：若在 select! 里反复取消 next_line，
    // 半行数据会丢失，超长工具输出还会把管道写满，导致 CLI 卡死。
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let stdout_task = match child.stdout.take() {
        Some(out) => {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            })
        }
        None => tokio::spawn(async {}),
    };
    drop(line_tx);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if core.interrupt_main.load(Ordering::SeqCst) {
                    interrupted = true;
                    let _ = child.kill().await;
                    break;
                }
            }
            line = line_rx.recv() => {
                match line {
                    Some(line) => handle_line(&core, &sid, &line, &mut final_text, &mut tool_log, &mut new_session),
                    None => break,
                }
            }
        }
    }
    let _ = stdout_task.await;
    let status = child.wait().await.ok();
    let stderr = stderr_task.await.unwrap_or_default();

    // 记住 OpenCode 会话 id，下一轮用 -s 续接上下文
    if let Some(new_id) = new_session {
        let mut map = oc_sessions().lock().await;
        map.insert(
            sid.clone(),
            OcSession {
                id: new_id,
                model: target.model.clone(),
            },
        );
    }

    if interrupted {
        for rec in tool_log.iter().filter(|r| !r.done) {
            core.emit(Event::ToolResult {
                session_id: sid.clone(),
                agent: "main".into(),
                name: rec.name.clone(),
                ok: false,
                summary: "已中断".into(),
            });
        }
        core.emit(Event::AssistantDone {
            session_id: sid.clone(),
            text: "（语音指令已打断本轮）".into(),
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    } else if final_text.trim().is_empty() && tool_log.is_empty() {
        // 没有任何输出：把 CLI 的 stderr 尾巴作为失败原因展示
        let tail: String = stderr
            .trim()
            .chars()
            .rev()
            .take(400)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let msg = if stderr.trim().is_empty() {
            format!(
                "OpenCode 没有返回内容（退出码 {:?}）",
                status.and_then(|s| s.code())
            )
        } else {
            format!("OpenCode 调用失败：{tail}")
        };
        persist_and_done(&core, &sid, &msg, tool_log, t0).await;
    } else {
        persist_and_done(&core, &sid, &final_text, tool_log, t0).await;
    }

    core.main_busy.store(false, Ordering::SeqCst);
    *core.turn_session.lock().await = None;
    core.set_main_status("idle");
    if !interrupted {
        crate::tools::auto_announce(core.clone(), final_text).await;
    }
}

/// 解析一行 CLI JSON 事件 → 项目事件
fn handle_line(
    core: &Arc<CoreState>,
    sid: &str,
    line: &str,
    final_text: &mut String,
    tool_log: &mut Vec<ToolCallRecord>,
    new_session: &mut Option<String>,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(s) = v.get("sessionID").and_then(|x| x.as_str()) {
        *new_session = Some(s.to_string());
    }
    match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        // text：part.text 是该步的完整快照，直接覆盖最终文本
        "text" => {
            if let Some(t) = v.pointer("/part/text").and_then(|x| x.as_str()) {
                if !t.is_empty() {
                    *final_text = t.to_string();
                    core.emit(Event::AssistantText {
                        session_id: sid.to_string(),
                        text: t.to_string(),
                    });
                }
            }
        }
        // tool_use：part.tool / part.state.{status,input,output}
        "tool_use" => {
            let part = v.get("part");
            let name = part
                .and_then(|p| p.get("tool"))
                .and_then(|x| x.as_str())
                .unwrap_or("tool")
                .to_string();
            let state = part.and_then(|p| p.get("state"));
            let input = state
                .and_then(|s| s.get("input"))
                .cloned()
                .unwrap_or(Value::Null);
            let status = state
                .and_then(|s| s.get("status"))
                .and_then(|x| x.as_str())
                .unwrap_or("running");
            let output = state
                .and_then(|s| s.get("output"))
                .and_then(|x| x.as_str())
                .unwrap_or("");

            core.emit(Event::ToolUse {
                session_id: sid.to_string(),
                agent: "main".into(),
                name: name.clone(),
                input: input.clone(),
            });

            let done = status == "completed" || status == "error" || status == "failed";
            if done {
                let summary: String = output.chars().take(300).collect();
                let ok = status == "completed";
                core.emit(Event::ToolResult {
                    session_id: sid.to_string(),
                    agent: "main".into(),
                    name: name.clone(),
                    ok,
                    summary: summary.clone(),
                });
                tool_log.push(ToolCallRecord {
                    name,
                    input,
                    ok,
                    summary,
                    done: true,
                });
            } else {
                tool_log.push(ToolCallRecord {
                    name,
                    input,
                    ok: true,
                    summary: String::new(),
                    done: false,
                });
            }
        }
        _ => {}
    }
}

/// 落盘 assistant 消息 + 广播完成（顺序固定：先落盘再 done，避免前端切会话读到空历史）
async fn persist_and_done(
    core: &Arc<CoreState>,
    sid: &str,
    text: &str,
    tool_log: Vec<ToolCallRecord>,
    t0: std::time::Instant,
) {
    if !text.is_empty() || !tool_log.is_empty() {
        let mut sessions = core.sessions.lock().await;
        sessions.append(
            sid,
            "assistant",
            text,
            tool_log,
            t0.elapsed().as_millis() as u64,
        );
    }
    core.emit(Event::AssistantDone {
        session_id: sid.to_string(),
        text: text.to_string(),
        elapsed_ms: t0.elapsed().as_millis() as u64,
    });
}
