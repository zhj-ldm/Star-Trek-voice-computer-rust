//! 全局配置：双 Agent API、语音阈值、skills、TTS 后端等。
//! 持久化到 data/config.json，UI 可读写。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CORE_PORT: u16 = 8410;
pub const DEFAULT_VOICE_PORT: u16 = 8420;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 数据目录（对话/记忆/定时任务/skills 配置）
    pub data_dir: PathBuf,

    // ---- 主 Agent ----
    pub main_base_url: String,
    pub main_api_key: String,
    pub main_model: String,
    pub main_system_prompt: String,

    // ---- 子 Agent ----
    pub sub_base_url: String,
    pub sub_api_key: String,
    pub sub_model: String,
    pub sub_system_prompt: String,

    // ---- 语音 ----
    pub voice_host: String,
    pub voice_port: u16,
    /// 唤醒词
    pub wakeword: String,
    /// 唤醒提示音
    pub beep_file: String,
    /// TTS 音色
    pub voice: String,
    /// 语速（1.0 为正常）
    pub rate: f32,
    /// TTS 后端: "internal"(edge-tts-rust) / "goose-tts"(外部二进制)
    pub tts_backend: String,
    /// 外部 TTS 二进制路径（goose-tts-copy）
    pub goose_tts_path: String,
    /// 打断关键词（识别到即打断当前任务）
    pub interrupt_keywords: Vec<String>,
    /// 录音最长秒数（用户说完自动截断）
    pub max_record_secs: f64,
    /// 唤醒词检测阈值（keywords_threshold，越低越灵敏）
    pub kws_threshold: f32,
    /// 语音交互开关
    pub voice_enabled: bool,

    // ---- Skills ----
    /// skills 根目录列表（每个 skill 一个子文件夹）
    pub skill_dirs: Vec<PathBuf>,

    // ---- 会话 ----
    /// 主 agent 最大轮数
    pub max_turns: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/zhj".into());
        Self {
            data_dir: PathBuf::from(format!("{home}/star-trek-assistant/data")),
            main_base_url: "https://api.openai.com/v1".into(),
            main_api_key: String::new(),
            main_model: "gpt-4o".into(),
            main_system_prompt: "You are Marvis, the ship computer of the starship Enterprise. You are the MAIN AGENT. \
Role boundary: your only jobs are web search, dispatching sub-agents via DispatchTask, monitoring their progress, and interacting with the user by voice. \
For any complete complex task (file operations, code, web interactions, etc.) you MUST delegate it to a sub-agent via DispatchTask. \
[VOICE ANNOUNCEMENT — HARD REQUIREMENT] After every user message, you MUST call SpeakToUser at least once before ending your turn. \
This is a non-negotiable hard rule: a turn that contains no announcement is NOT allowed to finish, no matter how short the reply is (acknowledgements, errors, task results are no exception). \
Normally call SpeakToUser exactly once per turn, announcing the complete core conclusion of that turn in one call — do not split it into repeated announcements. \
If a SpeakToUser call is rejected with \"speech is already playing\", this turn has already been announced — just finish your reply, do NOT retry the tool. \
[SUB-AGENT — SINGLE CONCURRENCY] Only one sub-agent task may run at a time. If DispatchTask is rejected with \"a sub-agent task is already running\", do NOT dispatch again — tell the user the current task is in progress and will be reported automatically when done. \
Keep replies concise, fast and professional, like crisp starship communication."
                .into(),
            sub_base_url: "https://api.openai.com/v1".into(),
            sub_api_key: String::new(),
            sub_model: "gpt-4o".into(),
            sub_system_prompt: "你是星际迷航语音助手的子 Agent，具备完整工具能力。\
你需要认真完成主 Agent 派发的任务，可以使用文件、搜索、代码等一切可用工具。\
任务完成后，用简洁中文总结关键结果。".into(),
            voice_host: "127.0.0.1".into(),
            voice_port: DEFAULT_VOICE_PORT,
            wakeword: "computer".into(),
            beep_file: "/Users/zhj/Projects/star-trek-assistant/resources/wake_sound.wav".into(),
            voice: "zh-CN-XiaoxiaoNeural".into(),
            rate: 1.05,
            tts_backend: "goose-tts".into(),
            goose_tts_path: "/Users/zhj/Projects/star-trek-assistant/resources/goose-tts".into(),
            interrupt_keywords: vec!["stop".into(), "停止".into(), "停".into(), "够了".into(), "取消".into()],
            max_record_secs: 120.0,
            kws_threshold: 0.15,
            voice_enabled: false,
            skill_dirs: vec![PathBuf::from(format!("{home}/Desktop/goose-tts-copy"))],
            max_turns: 1000,
        }
    }
}

impl Config {
    pub fn voice_base(&self) -> String {
        format!("http://{}:{}", self.voice_host, self.voice_port)
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("解析 config.json 失败，使用默认配置: {e}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }
}
