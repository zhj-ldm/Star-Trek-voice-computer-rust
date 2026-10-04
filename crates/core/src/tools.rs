//! 主 Agent 自定义工具：语音播报、派发子 Agent、打断、监控、导入 skill。
//! 这些工具是主 Agent 能力的全部来源（配合 WebSearch）。

use crate::events::Event;
use crate::state::CoreState;
use async_trait::async_trait;
use open_agent_sdk::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn str_of(input: &Value, key: &str, default: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

// ============================================================
// auto_announce —— core 自动播报最终回复（不走 AI 工具调用）
// 由 run_main_turn 在每轮结束后直接调用：用本轮最终文本合成并播放。
// TTS 后台异步执行，不阻塞 agent 循环；speaking 原子标志与
// speak_start/speak_end 事件在后台任务内管理，前端展示语义不变。
// ============================================================

/// 清洗 Markdown 标记，避免 TTS 把 `**`、`*`、`#`、反引号、链接等念出来。
/// 逐行处理：行首去掉标题/引用/列表/代码围栏标记；行内去掉强调符，
/// 并把 `[文字](url)` 还原为纯文字。
/// 从最终回复中提取「语音播报」段（提示词约定：回复末尾以 【语音播报】 开头的一段，
/// 专供 TTS 朗读，口语化 ≤100 字）。无标记时回退播报全文（兼容旧回复/未遵守格式的情况）。
/// 注意：不在此处强行截断——长内容必须完整念完，避免播报中途戛然而止。
fn extract_announce_text(raw: &str) -> String {
    // 提示词约定标记：新版用英文【Voice】（星舰风格），同时兼容旧版【语音播报】。
    const MARKER: &str = "【Voice】";
    const LEGACY: &str = "【语音播报】";
    // 找到标记 → 只取标记后的朗读内容；标记后为空（空标记）也回退全文，
    // 保证用户最终总能听到内容；无标记 → 回退 AI 最终回复全文。
    let text = if let Some(pos) = raw.find(MARKER) {
        let t = raw[pos + MARKER.len()..].trim().to_string();
        if t.is_empty() {
            raw.trim().to_string()
        } else {
            t
        }
    } else if let Some(pos) = raw.find(LEGACY) {
        let t = raw[pos + LEGACY.len()..].trim().to_string();
        if t.is_empty() {
            raw.trim().to_string()
        } else {
            t
        }
    } else {
        raw.trim().to_string()
    };
    strip_markdown_for_tts(&text).trim().to_string()
}

fn strip_markdown_for_tts(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut l = line.trim().to_string();
        let trimmed = l.trim_start();
        let mut chars = trimmed.chars();
        let first = chars.next();
        let second = chars.next();
        // 行首标记：# 标题、-/* 列表、> 引用、| 表格、` 代码围栏
        let is_line_mark = matches!(
            (first, second),
            (Some('#'), _) | (Some('-'), _) | (Some('*'), _) | (Some('＊'), _) | (Some('>'), _) | (Some('|'), _) | (Some('`'), _)
        );
        // 数字列表：1. / 1、 / 1) 开头
        let mut is_num_mark = false;
        if let Some(c) = first {
            if c.is_ascii_digit() {
                let mut it = trimmed.chars().skip(1).peekable();
                let mut digits = 1;
                while let Some(&d) = it.peek() {
                    if d.is_ascii_digit() {
                        digits += 1;
                        it.next();
                    } else {
                        break;
                    }
                }
                if let Some(&d) = it.peek() {
                    if matches!(d, '.' | '、' | ')') && digits > 0 {
                        is_num_mark = true;
                    }
                }
            }
        }
        if is_line_mark || is_num_mark {
            l = trimmed
                .chars()
                .skip_while(|c| {
                    matches!(c, '#' | '-' | '*' | '>' | '|' | '`')
                        || c.is_ascii_digit()
                        || matches!(c, '.' | '、' | ')' | ' ' | '\t')
                })
                .collect::<String>();
        }
        // 行内：去掉强调/代码标记，链接 `[文字](url)` 还原为文字
        let mut cleaned = String::with_capacity(l.len());
        let mut it = l.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                // ASCII 强调符 + 全角星号/序号符等 TTS 会念成"星号/杠"的符号，一律跳过
                '*' | '_' | '~' | '`' | '＊' | '※' | '·' | '•' | '◇' | '◆' | '→' | '→' => { /* 跳过 */ }
                '[' => {
                    let mut buf = String::new();
                    let mut found = false;
                    while let Some(&n) = it.peek() {
                        if n == ']' {
                            it.next();
                            if it.peek() == Some(&'(') {
                                it.next();
                                while let Some(&m) = it.peek() {
                                    if m == ')' {
                                        it.next();
                                        found = true;
                                        break;
                                    }
                                    it.next();
                                }
                            }
                            break;
                        }
                        buf.push(n);
                        it.next();
                    }
                    if found {
                        cleaned.push_str(&buf);
                    } else {
                        cleaned.push('[');
                        cleaned.push_str(&buf);
                    }
                }
                _ => cleaned.push(c),
            }
        }
        lines.push(cleaned);
    }
    lines.join("\n")
}

pub async fn auto_announce(core: Arc<CoreState>, text: String) {
    let text = extract_announce_text(&text);
    tracing::info!("auto_announce text ({} chars): {}", text.chars().count(), text);
    if text.is_empty() {
        return;
    }
    // 互斥：已有语音正在播报时跳过本次（不排队、不累积）
    if core.speaking.load(Ordering::SeqCst) {
        tracing::info!("自动播报跳过：已有语音正在播报");
        return;
    }
    core.emit(Event::Voice {
        kind: "speak_start".into(),
        text: text.clone(),
    });
    core.speaking.store(true, Ordering::SeqCst);

    // TTS 后台异步播报，不阻塞主 Agent 循环（main_busy 只覆盖 LLM+工具逻辑）
    tokio::spawn(async move {
        let cfg = {
            let c = core.config.lock().await;
            (
                c.tts_backend.clone(),
                c.voice.clone(),
                c.rate,
                c.goose_tts_path.clone(),
            )
        };
        let text2 = text.clone();
        let result: Result<(), String> = match cfg.0.as_str() {
            "goose-tts" => {
                // 统一可打断播放：先合成到临时 mp3（不直接播放），再交给
                // voice-serve 的 rodio 播放器播放。这样 /interrupt 能真正打断
                // TTS（旧实现直接跑 goose-tts 子进程，暂停按钮对它无效），
                // 且 KWS 在播放期间自动停检（busy），避免播报内容被自己唤醒。
                let (bin, voice, rate) = (cfg.3.clone(), cfg.1.clone(), cfg.2);
                let tmp = std::env::temp_dir().join(format!(
                    "star-speak-{}.mp3",
                    uuid::Uuid::new_v4()
                ));
                let out = tmp.to_string_lossy().into_owned();
                let out2 = out.clone();
                let text3 = text2.clone();
                let core2 = core.clone();
                let synth = tokio::task::spawn_blocking(move || {
                    core2
                        .voice
                        .speak_goose_tts_to_file(&bin, &text3, &voice, rate, &out2)
                })
                .await;
                let synth = match synth {
                    Ok(r) => r.map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                // 打断缺口修复：合成期间用户点了暂停/说打断词 → 已合成的音频
                // 也不得再播放（旧实现合成完成后照播，表现为"打断后 AI 还在说"）
                let interrupted = core.interrupt_main.load(Ordering::SeqCst);
                let play = match (synth, interrupted) {
                    (Ok(_), false) => core.voice.beep(Some(&out)).await,
                    (Ok(_), true) => {
                        tracing::info!("打断生效：跳过已合成的待播报语音");
                        Ok(())
                    }
                    (Err(e), _) => Err(anyhow::anyhow!(e)),
                };
                let _ = std::fs::remove_file(&out);
                play.map_err(|e| e.to_string())
            }
            _ => {
                if core.interrupt_main.load(Ordering::SeqCst) {
                    tracing::info!("打断生效：跳过 edge-tts 播报");
                    Ok(())
                } else {
                    core.voice
                        .speak(&text2, &cfg.1, cfg.2)
                        .await
                        .map_err(|e| e.to_string())
                }
            }
        };
        core.speaking.store(false, Ordering::SeqCst);
        core.emit(Event::Voice {
            kind: "speak_end".into(),
            text: text2,
        });
        if let Err(e) = result {
            // 播报失败：不重试（避免死循环），仅记录日志
            tracing::warn!("TTS 自动播报失败: {e}");
        }
    });
}

// ============================================================
// ImportSkill —— 让 AI 导入 skill 文件夹
// ============================================================

pub struct ImportSkill {
    core: Arc<CoreState>,
}

impl ImportSkill {
    pub fn new(core: Arc<CoreState>) -> Self {
        Self { core }
    }
}

#[async_trait]
impl Tool for ImportSkill {
    fn name(&self) -> &str {
        "ImportSkill"
    }
    fn description(&self) -> &str {
        "导入一个 skill 文件夹到 skills 系统。skill 文件夹需包含运行文件与说明文档（如 README.md）。\
输入文件夹绝对路径即可。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([(
                "path".to_string(),
                json!({"type": "string", "description": "skill 文件夹的绝对路径"}),
            )]),
            required: vec!["path".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let path = str_of(&input, "path", "");
        if path.is_empty() {
            return Ok(ToolResult::error("缺少路径 path"));
        }
        match self.core.skills.import(&path).await {
            Ok(skill) => {
                self.core.emit(Event::SkillsUpdated);
                Ok(ToolResult::text(format!(
                    "已导入 skill「{}」：{}",
                    skill.name, skill.description
                )))
            }
            Err(e) => Ok(ToolResult::error(format!("导入失败: {e}"))),
        }
    }
}

// ============================================================
// DeliverFiles —— AI 主动登记本轮交付物（前端据此渲染"交付卡片"）
// ============================================================

pub struct DeliverFiles;

#[async_trait]
impl Tool for DeliverFiles {
    fn name(&self) -> &str {
        "DeliverFiles"
    }
    fn description(&self) -> &str {
        "把本轮交付给用户的成品文件登记为交付物清单，用于在回复区生成交付卡片。\
本轮产出或修改了文件、或需要把结果以文件形式交给用户时，必须调用本工具登记（绝对路径 + 一句话说明）。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "title".to_string(),
                    json!({"type": "string", "description": "卡片标题，如「已编辑 8 个文件」；缺省时前端用「已交付 N 个文件」"}),
                ),
                (
                    "files".to_string(),
                    json!({
                        "type": "array",
                        "description": "交付物文件列表",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "文件绝对路径"},
                                "title": {"type": "string", "description": "显示名（缺省用文件名）"},
                                "desc": {"type": "string", "description": "一句话说明"}
                            },
                            "required": ["path"]
                        }
                    }),
                ),
            ]),
            required: vec!["files".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let files = input
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            return Ok(ToolResult::error(
                "files 为空：请提供至少一个交付物 {path, title?, desc?}",
            ));
        }
        let n = files.len();
        let names: Vec<String> = files
            .iter()
            .take(8)
            .map(|f| {
                let p = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
                f.get("title")
                    .and_then(|v| v.as_str())
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| p.rsplit('/').next().unwrap_or(p).to_string())
            })
            .collect();
        Ok(ToolResult::text(format!(
            "已交付 {} 个文件：{}",
            n,
            names.join("、")
        )))
    }
}
